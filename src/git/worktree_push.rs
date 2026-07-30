//! Batch-publish a repository's worktrees to their upstreams, force-pushing with a
//! lease where history was rewritten (issue #1443).
//!
//! This is the other half of [`worktree_rebase`]. A rebase rewrites history, so
//! every already-published branch it touches is now diverged from its upstream and
//! a plain `git push` is rejected — leaving the user to open a terminal in each
//! worktree and force-push by hand. This engine closes that loop for a whole batch.
//!
//! **Split of concerns (see ADR-0003 and ADR-0061).** Every git *read* — the
//! checked-out branch, the upstream and its remote, the divergence, the repository's
//! remote default branch — goes through `git2`. The one *mutation*, the push itself,
//! shells out to the user's `git`: libgit2's vendored build here has no reliable SSH
//! transport (issue #903), the shell inherits the user's `ssh-agent` /
//! `~/.ssh/config` / credential-helper configuration for free, and only real `git`
//! implements the lease this engine depends on. The binary is resolved through
//! [`crate::git::resolve_git_binary`] rather than by name, because the daemon's
//! `PATH` is minimal (ADR-0059 §3).
//!
//! # Two rules this engine exists to enforce
//!
//! **1. Always `--force-with-lease --force-if-includes`, never `--force`.**
//! A bare `--force-with-lease` leases against the *local* remote-tracking ref, so
//! any background fetch that refreshes `refs/remotes/<remote>/<branch>` silently
//! renews the lease and a teammate's unseen commit becomes overwritable. Per
//! `git-push(1)`, `--force-if-includes` additionally verifies that such implicitly
//! updated remote-tracking refs were actually integrated locally — and it is **not**
//! implied: it must be passed explicitly, and it is a documented no-op unless
//! `--force-with-lease` is given in its valueless (or refname-only) form. The hazard
//! is elevated in exactly the environment this ships into, since the built-in VS
//! Code Git extension's `git.autofetch` is precisely such a background fetch.
//! `--force` is never emitted, and no option exposes it: a refused lease is the
//! feature working.
//!
//! **2. Never force-push the repository's remote default branch.** [ADR-0060]
//! dropped the rebase engine's main-working-tree gate; a force-push must invert
//! that, and the gate is on the *branch* rather than the worktree's structural
//! role. A rebase rewrites only local history and is `git reflog`-recoverable; a
//! force-push publishes that rewrite to everyone. A fast-forward onto the default
//! branch stays allowed, because it is an ordinary push.
//!
//! # Planning does not touch the network
//!
//! Unlike [`worktree_rebase::plan`], [`plan`] performs no fetch — which is why it
//! takes no `git` binary at all. Classification reads the local
//! `refs/remotes/<remote>/<branch>`, and that is *exactly* the ref the lease is
//! checked against, so the plan the user confirms and the lease the push enforces
//! agree by construction. A fetch here would refresh that ref — renewing the very
//! lease rule 1 exists to protect. The cost is that a teammate's unseen push reads
//! as [`PushResult::WouldFastForward`] and is then refused by the remote, which is
//! reported rather than forced.
//!
//! [`worktree_rebase`]: crate::git::worktree_rebase
//! [`worktree_rebase::plan`]: crate::git::worktree_rebase::plan
//! [ADR-0060]: https://github.com/rust-works/omni-dev/blob/main/docs/adrs/adr-0060.md

use std::path::{Path, PathBuf};

use anyhow::Result;
use git2::{Oid, Repository};
use serde::Serialize;

use crate::git::remote::RemoteInfo;
use crate::git::resolve_git_binary;
use crate::git::worktree_batch::{
    head_branch, is_false, resolve_selection, run_git_in, trimmed_stderr,
};

pub use crate::git::worktree_batch::Selection;

/// The remote a branch with no configured upstream is published to, when the
/// repository has one by that name. Matches git's own convention and
/// [`crate::git::repository::GitRepository::push_branch`].
const DEFAULT_REMOTE: &str = "origin";

/// Knobs for a batch push.
///
/// Deliberately minimal: there is no force escape hatch (see the module docs) and
/// no remote override — a branch publishes to its own upstream's remote, which is
/// the only destination this engine will use.
#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// The `git` executable to shell out to. `None` resolves it via
    /// [`resolve_git_binary`], which is what a caller with a minimal `PATH` (the
    /// daemon) needs; the field exists so a caller can resolve **once** and reuse,
    /// and so a test can point the engine at a stub.
    pub git_bin: Option<PathBuf>,
}

impl PushOptions {
    /// The `git` executable these options select, resolving the default lazily.
    /// Called once per [`execute`] rather than per subprocess.
    fn git_bin(&self) -> PathBuf {
        self.git_bin.clone().unwrap_or_else(resolve_git_binary)
    }
}

/// The result of planning and (optionally) executing a batch push.
///
/// Unlike [`worktree_rebase::Plan`](crate::git::worktree_rebase::Plan) there is no
/// `fetches` field: planning contacts no remote (see the module docs).
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// One entry per selected worktree, in selection order.
    pub worktrees: Vec<WorktreeOutcome>,
}

impl Plan {
    /// Whether any worktree still has something to publish. The CLI uses this to
    /// decide whether to confirm and execute.
    #[must_use]
    pub fn has_pending_pushes(&self) -> bool {
        self.worktrees.iter().any(|w| w.result.is_pending())
    }
}

/// What happened (or, in a plan, would happen) to one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeOutcome {
    /// The worktree folder, canonicalized — the key the daemon's transient
    /// `pushing` set is joined on.
    pub path: PathBuf,
    /// The checked-out branch, when on one (absent for a detached/unborn HEAD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The remote this branch publishes to (empty when it could not be resolved).
    pub remote: String,
    /// The branch name on the remote. Usually identical to `branch`, but
    /// `branch.<name>.merge` may name a different ref. Empty when unresolved.
    pub remote_branch: String,
    /// The classification / outcome.
    #[serde(flatten)]
    pub result: PushResult,
}

/// The per-worktree classification and outcome.
///
/// [`plan`] only ever produces `UpToDate` / `WouldFastForward` / `WouldForce` /
/// `WouldCreate` / `Skipped`; [`execute`] turns each pending variant into `Pushed`,
/// `Created` or `Rejected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum PushResult {
    /// The upstream is already at the local tip — nothing to publish.
    UpToDate,
    /// Ahead of the upstream and not behind it: a plain push suffices. **No force
    /// flag is used**, on the default branch included.
    WouldFastForward {
        /// Commits the branch is ahead of its upstream.
        ahead: usize,
    },
    /// Diverged from the upstream — ahead *and* behind — so the push needs the
    /// lease. This is what a rebase leaves behind, and the only variant the
    /// default-branch gate refuses.
    WouldForce {
        /// Commits the branch is ahead of its upstream.
        ahead: usize,
        /// Commits the branch is behind its upstream.
        behind: usize,
    },
    /// The branch has no upstream, so publishing it creates one
    /// (`--set-upstream`). Included deliberately: a batch action that silently
    /// refused every unpublished branch would be worse than one that publishes it,
    /// and `--force-with-lease` is a no-op against a ref that does not exist yet.
    WouldCreate,
    /// Published to the existing upstream.
    Pushed {
        /// Whether this was a leased non-fast-forward update rather than an
        /// ordinary fast-forward.
        forced: bool,
    },
    /// Published a branch that had no upstream, and recorded the tracking ref.
    Created,
    /// The remote refused the update, or `git push` failed.
    Rejected {
        /// The refusal reason, from `git push --porcelain` where it gave one, else
        /// the trimmed `git` error.
        detail: String,
        /// Whether the refusal was the **lease** — the remote moved since the tip
        /// this worktree last saw. The signal that the fix is `git fetch` plus a
        /// rebase, never a harder push. Omitted on the wire when false, so a client
        /// that does not know the field sees the same bytes.
        #[serde(skip_serializing_if = "is_false")]
        stale: bool,
    },
    /// Skipped without contacting the remote, for a structural reason.
    Skipped {
        /// Why it was skipped.
        reason: SkipReason,
    },
}

impl PushResult {
    /// Whether this outcome still has something to publish — i.e. whether
    /// [`execute`] will act on it.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending_kind().is_some()
    }

    /// The flavour of push this outcome calls for, or `None` when there is nothing
    /// to do. The single place the classification-to-flags mapping lives.
    const fn pending_kind(&self) -> Option<PushKind> {
        match self {
            Self::WouldFastForward { .. } => Some(PushKind::FastForward),
            Self::WouldForce { .. } => Some(PushKind::Force),
            Self::WouldCreate => Some(PushKind::Create),
            _ => None,
        }
    }
}

/// Why a worktree was skipped rather than pushed.
///
/// A **dirty** worktree is deliberately absent: a push publishes commits, not the
/// working tree, so uncommitted changes are no reason to refuse one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// A detached or unborn HEAD — there is no branch to publish. This is also
    /// what covers a worktree sitting mid-rebase, whose HEAD *is* detached.
    DetachedHead,
    /// The path is not a git worktree.
    NotAWorktree,
    /// The branch tracks no upstream and the repository has no remote to publish
    /// to, so there is no destination to create one on.
    NoRemote,
    /// The branch is the repository's remote default branch and the push would be
    /// a non-fast-forward. Refused outright: a rebase is local and
    /// `git reflog`-recoverable, but force-pushing the default branch publishes
    /// that rewrite to everyone (ADR-0061).
    DefaultBranchForcePush,
}

/// The three shapes a push can take, each with its own flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushKind {
    /// Ahead only — an ordinary push, no force flags.
    FastForward,
    /// Diverged — `--force-with-lease --force-if-includes`.
    Force,
    /// No upstream yet — `--set-upstream`.
    Create,
}

/// Plans a batch push, classifying every selected worktree against its upstream.
///
/// **Contacts no remote**, which is why it needs no `git` binary: the comparison is
/// against the local `refs/remotes/<remote>/<branch>`, the same ref
/// `--force-with-lease` leases against, so the plan and the lease agree by
/// construction. The returned [`Plan`] is exactly what a dry run reports; a real
/// run passes it to [`execute`].
///
/// # Errors
///
/// Returns an error only when the selection itself cannot be resolved (e.g.
/// `--all` outside a repository). A single unusable path is reported as a
/// [`SkipReason`], never an error, so one bad path cannot fail the whole batch.
pub fn plan(selection: &Selection) -> Result<Plan> {
    let paths = resolve_selection(selection)?;
    let worktrees = paths.iter().map(|path| classify(path)).collect();
    Ok(Plan { worktrees })
}

/// Executes a [`Plan`], publishing every worktree that still has something to push.
///
/// The rest pass through unchanged. Pushes run sequentially: linked worktrees share
/// one object database and ref store, and a successful push writes
/// `refs/remotes/<remote>/<branch>` into it. A refused push is reported and the
/// batch continues with the remaining worktrees.
#[must_use]
pub fn execute(plan: Plan, opts: &PushOptions) -> Vec<WorktreeOutcome> {
    let git = opts.git_bin();
    plan.worktrees
        .into_iter()
        .map(|mut outcome| {
            if let Some(kind) = outcome.result.pending_kind() {
                outcome.result = push_worktree(&git, &outcome, kind);
            }
            outcome
        })
        .collect()
}

// ── classification (git2 reads only) ─────────────────────────────────────────

/// Reads and classifies one selected path.
///
/// A single pass, unlike the rebase engine's inspect-then-classify split: that
/// split exists only to group worktrees by repository for its fetch-once contract,
/// and there is no fetch here to group.
fn classify(path: &Path) -> WorktreeOutcome {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Ok(repo) = Repository::discover(&canon) else {
        return WorktreeOutcome::skipped(canon, None, SkipReason::NotAWorktree);
    };

    // Safe-before-destructive order, exactly as the rebase classifier.
    let (branch, head_oid) = head_branch(&repo);
    let (Some(branch), Some(head)) = (branch, head_oid) else {
        return WorktreeOutcome::skipped(canon, None, SkipReason::DetachedHead);
    };

    let Some(target) = resolve_target(&repo, &branch) else {
        return WorktreeOutcome::skipped(canon, Some(branch), SkipReason::NoRemote);
    };

    let outcome = |result| WorktreeOutcome {
        path: canon.clone(),
        branch: Some(branch.clone()),
        remote: target.remote.clone(),
        remote_branch: target.remote_branch.clone(),
        result,
    };

    // No upstream yet: publishing creates one. `--force-with-lease` cannot apply to
    // a ref that does not exist, so the default-branch gate below is irrelevant
    // here — creating a branch destroys nothing.
    let Some(upstream_oid) = target.upstream_oid else {
        return outcome(PushResult::WouldCreate);
    };

    let Some((ahead, behind)) = repo.graph_ahead_behind(head, upstream_oid).ok() else {
        // Either tip is unreachable in this object database — treat it as nothing
        // to publish rather than guessing at a force.
        return outcome(PushResult::UpToDate);
    };

    match (ahead, behind) {
        (0, _) => outcome(PushResult::UpToDate),
        (ahead, 0) => outcome(PushResult::WouldFastForward { ahead }),
        (ahead, behind) => {
            // The one gate that inverts ADR-0060: refuse to *force*-push the
            // repository's remote default branch, whichever worktree holds it. A
            // fast-forward onto it (above) stays an ordinary push and is allowed.
            if is_default_branch(&repo, &target) {
                return outcome(PushResult::Skipped {
                    reason: SkipReason::DefaultBranchForcePush,
                });
            }
            outcome(PushResult::WouldForce { ahead, behind })
        }
    }
}

/// Where a branch publishes to, and what its upstream currently points at.
struct Target {
    /// The remote name (`origin`, or whatever the branch tracks).
    remote: String,
    /// The branch name **on the remote** — usually the local name, but
    /// `branch.<name>.merge` may say otherwise.
    remote_branch: String,
    /// The commit the remote-tracking ref points at, or `None` when the branch
    /// tracks no upstream yet.
    upstream_oid: Option<Oid>,
}

/// Resolves a branch's push destination.
///
/// With a configured upstream the remote comes from `branch.<name>.remote` and the
/// destination ref from `branch.<name>.merge` — the same two values git's own
/// default push resolution reads — while the tip to compare against comes from the
/// remote-tracking ref git2 resolves, which honours the remote's fetch refspec.
/// Without an upstream, the branch publishes to [`DEFAULT_REMOTE`] if the
/// repository has it, else to its single remote; `None` when it has neither.
fn resolve_target(repo: &Repository, branch: &str) -> Option<Target> {
    let refname = format!("refs/heads/{branch}");

    if let Some(remote) = buf_string(repo.branch_upstream_remote(&refname)) {
        let upstream_oid = buf_string(repo.branch_upstream_name(&refname))
            .and_then(|name| repo.refname_to_id(&name).ok());
        return Some(Target {
            remote_branch: merge_ref_branch(repo, branch).unwrap_or_else(|| branch.to_string()),
            remote,
            upstream_oid,
        });
    }

    Some(Target {
        remote: fallback_remote(repo)?,
        remote_branch: branch.to_string(),
        upstream_oid: None,
    })
}

/// An owned `String` from a git2 `Buf`-returning call, dropping both the git error
/// and the (vanishingly rare) non-UTF-8 case in one step.
fn buf_string(buf: std::result::Result<git2::Buf, git2::Error>) -> Option<String> {
    buf.ok()?.as_str().ok().map(ToString::to_string)
}

/// The short branch name `branch.<name>.merge` points at, when it names an ordinary
/// `refs/heads/*` ref. `None` for an unset or exotic value, leaving the caller to
/// publish under the local name.
fn merge_ref_branch(repo: &Repository, branch: &str) -> Option<String> {
    let merge = repo
        .config()
        .ok()?
        .get_string(&format!("branch.{branch}.merge"))
        .ok()?;
    merge.strip_prefix("refs/heads/").map(ToString::to_string)
}

/// The remote an upstream-less branch publishes to: [`DEFAULT_REMOTE`] when the
/// repository has it, else its sole remote. `None` when the repository has no
/// remotes, or several and no `origin` — in which case the destination is genuinely
/// ambiguous and the worktree is skipped rather than guessed at.
fn fallback_remote(repo: &Repository) -> Option<String> {
    let remotes = repo.remotes().ok()?;
    // `iter()` yields `Result<Option<&str>, _>`: the first `flatten` drops per-name
    // errors, the second drops non-UTF-8 names (the same idiom the worktree
    // enumeration in `worktree_batch` uses).
    let names: Vec<String> = remotes
        .iter()
        .flatten()
        .flatten()
        .map(String::from)
        .collect();
    if names.iter().any(|name| name == DEFAULT_REMOTE) {
        return Some(DEFAULT_REMOTE.to_string());
    }
    match names.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Whether this push targets the remote's default branch — the one branch a force
/// push is refused on. Compared on the **remote-side** name, which is what is
/// actually being protected.
fn is_default_branch(repo: &Repository, target: &Target) -> bool {
    RemoteInfo::detect_main_branch_local(repo, &target.remote)
        .is_some_and(|default| default == target.remote_branch)
}

impl WorktreeOutcome {
    /// A `Skipped` outcome, for the cases where no destination was resolved.
    fn skipped(path: PathBuf, branch: Option<String>, reason: SkipReason) -> Self {
        Self {
            path,
            branch,
            remote: String::new(),
            remote_branch: String::new(),
            result: PushResult::Skipped { reason },
        }
    }
}

// ── push (shell-out, per worktree) ───────────────────────────────────────────

/// Publishes one worktree's branch, mapping `git push --porcelain`'s status line
/// onto a [`PushResult`].
fn push_worktree(git: &Path, outcome: &WorktreeOutcome, kind: PushKind) -> PushResult {
    let branch = outcome.branch.as_deref().unwrap_or_default();
    let args = push_args(kind, &outcome.remote, branch, &outcome.remote_branch);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    let output = match run_git_in(git, &outcome.path, &argv) {
        Ok(output) => output,
        Err(err) => {
            return PushResult::Rejected {
                detail: err.to_string(),
                stale: false,
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(result) = parse_porcelain(&stdout).into_iter().find_map(result_of) {
        return result;
    }
    // `--porcelain` always emits a status line, so this is the "git died before it
    // said anything" path: trust the exit status and report stderr.
    if output.status.success() {
        return match kind {
            PushKind::Create => PushResult::Created,
            PushKind::Force => PushResult::Pushed { forced: true },
            PushKind::FastForward => PushResult::Pushed { forced: false },
        };
    }
    PushResult::Rejected {
        detail: trimmed_stderr(&output),
        stale: false,
    }
}

/// The `git push` argument vector for one worktree. Pure, so the flag rules — the
/// load-bearing part of this engine — are unit-testable without a remote.
///
/// `--force-with-lease` is passed **valueless**: per `git-push(1)` the
/// `=<refname>:<expect>` form makes `--force-if-includes` a documented no-op, which
/// would silently drop the protection against a background fetch having renewed the
/// lease. `--force` is never emitted.
///
/// The refspec is fully qualified on both sides so neither end depends on
/// `push.default` or on the remote resolving a short name.
fn push_args(kind: PushKind, remote: &str, local: &str, remote_branch: &str) -> Vec<String> {
    let mut args = vec!["push".to_string(), "--porcelain".to_string()];
    match kind {
        PushKind::FastForward => {}
        PushKind::Force => {
            args.push("--force-with-lease".to_string());
            args.push("--force-if-includes".to_string());
        }
        PushKind::Create => args.push("--set-upstream".to_string()),
    }
    args.push(remote.to_string());
    args.push(format!("refs/heads/{local}:refs/heads/{remote_branch}"));
    args
}

/// One parsed `git push --porcelain` status line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainLine {
    /// The single-character status flag (see [`result_of`]).
    flag: char,
    /// The human summary, e.g. `[rejected]` or `abc123..def456`.
    summary: String,
    /// The parenthesised explanation, when git gave one.
    reason: Option<String>,
}

/// Parses `git push --porcelain` output (which goes to **stdout**) into its status
/// lines, dropping the `To <url>` header, the trailing `Done`, and anything else
/// that is not `<flag>\t<from>:<to>\t<summary> (<reason>)`.
fn parse_porcelain(stdout: &str) -> Vec<PorcelainLine> {
    stdout.lines().filter_map(parse_porcelain_line).collect()
}

/// Parses one status line, or `None` when the line is not one.
fn parse_porcelain_line(line: &str) -> Option<PorcelainLine> {
    let mut fields = line.split('\t');
    let flag_field = fields.next()?;
    let _refs = fields.next()?;
    let rest = fields.next()?;

    // The flag field is exactly one character — a space for a plain fast-forward,
    // which is why this cannot be written as a `trim` + `chars().next()`.
    let mut chars = flag_field.chars();
    let flag = chars.next()?;
    if chars.next().is_some() {
        return None;
    }

    let (summary, reason) = split_reason(rest);
    Some(PorcelainLine {
        flag,
        summary,
        reason,
    })
}

/// Splits `<summary> (<reason>)` into its two halves.
fn split_reason(rest: &str) -> (String, Option<String>) {
    let rest = rest.trim();
    if let Some(stripped) = rest.strip_suffix(')') {
        if let Some(open) = stripped.rfind('(') {
            return (
                stripped[..open].trim().to_string(),
                Some(stripped[open + 1..].trim().to_string()),
            );
        }
    }
    (rest.to_string(), None)
}

/// Maps one status line onto a [`PushResult`], or `None` for a flag this engine
/// never produces (a deletion) so the caller keeps looking.
///
/// The flag alphabet is `git-push(1)`'s: a space for a fast-forward, `+` for a
/// forced update, `*` for a new ref, `=` for up-to-date, `!` for a rejection.
fn result_of(line: PorcelainLine) -> Option<PushResult> {
    match line.flag {
        '=' => Some(PushResult::UpToDate),
        ' ' => Some(PushResult::Pushed { forced: false }),
        '+' => Some(PushResult::Pushed { forced: true }),
        '*' => Some(PushResult::Created),
        '!' => {
            let reason = line.reason.unwrap_or(line.summary);
            Some(PushResult::Rejected {
                stale: is_lease_refusal(&reason),
                detail: reason,
            })
        }
        _ => None,
    }
}

/// Whether a rejection reason is the **lease** refusing, rather than an ordinary
/// non-fast-forward or a server-side hook.
///
/// Git words the two lease failures differently — `stale info` when the
/// remote-tracking ref no longer matches the remote, and `remote ref updated since
/// checkout` when `--force-if-includes` finds an unintegrated update — and both
/// mean the same thing to the user: fetch, rebase, and try again.
fn is_lease_refusal(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("stale info") || reason.contains("remote ref updated since checkout")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use crate::git::worktree_batch::test_serial_lock;

    /// The shared git-load serialization guard (see
    /// [`crate::git::worktree_batch::test_serial_lock`]).
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        test_serial_lock()
    }

    // ── the flag rules (pure) ─────────────────────────────────────────────

    #[test]
    fn a_fast_forward_push_uses_no_force_flag_at_all() {
        assert_eq!(
            push_args(PushKind::FastForward, "origin", "feat", "feat"),
            vec![
                "push",
                "--porcelain",
                "origin",
                "refs/heads/feat:refs/heads/feat"
            ],
        );
    }

    #[test]
    fn a_forced_push_pairs_the_lease_with_force_if_includes() {
        let args = push_args(PushKind::Force, "origin", "feat", "feat");
        assert_eq!(
            args,
            vec![
                "push",
                "--porcelain",
                "--force-with-lease",
                "--force-if-includes",
                "origin",
                "refs/heads/feat:refs/heads/feat"
            ],
        );
        // The load-bearing detail (ADR-0061 §2): `--force-if-includes` is a
        // documented no-op unless `--force-with-lease` is *valueless*, so an
        // `=<refname>:<expect>` form would silently drop the protection.
        assert!(
            args.iter().all(|a| !a.starts_with("--force-with-lease=")),
            "the lease must stay valueless or --force-if-includes is a no-op"
        );
    }

    #[test]
    fn no_push_flavour_ever_emits_bare_force() {
        for kind in [PushKind::FastForward, PushKind::Force, PushKind::Create] {
            let args = push_args(kind, "origin", "feat", "feat");
            assert!(
                !args.iter().any(|a| a == "--force" || a == "-f"),
                "{kind:?} must never force without a lease"
            );
        }
    }

    #[test]
    fn creating_a_branch_sets_its_upstream() {
        assert_eq!(
            push_args(PushKind::Create, "upstream", "feat", "feat"),
            vec![
                "push",
                "--porcelain",
                "--set-upstream",
                "upstream",
                "refs/heads/feat:refs/heads/feat"
            ],
        );
    }

    #[test]
    fn the_refspec_honours_a_differing_remote_branch_name() {
        let args = push_args(PushKind::Force, "origin", "local-name", "remote-name");
        assert_eq!(
            args.last().unwrap(),
            "refs/heads/local-name:refs/heads/remote-name",
            "a `branch.<n>.merge` pointing elsewhere must not be pushed over the local name"
        );
    }

    // ── porcelain parsing (pure) ──────────────────────────────────────────

    #[test]
    fn porcelain_parsing_maps_every_documented_flag() {
        let cases = [
            (
                "=\trefs/heads/a:refs/heads/a\t[up to date]",
                PushResult::UpToDate,
            ),
            (
                " \trefs/heads/a:refs/heads/a\tabc123..def456",
                PushResult::Pushed { forced: false },
            ),
            (
                "+\trefs/heads/a:refs/heads/a\tabc123...def456 (forced update)",
                PushResult::Pushed { forced: true },
            ),
            (
                "*\trefs/heads/a:refs/heads/a\t[new branch]",
                PushResult::Created,
            ),
        ];
        for (line, expected) in cases {
            let parsed = parse_porcelain(line);
            assert_eq!(parsed.len(), 1, "failed to parse {line:?}");
            assert_eq!(result_of(parsed[0].clone()), Some(expected), "for {line:?}");
        }
    }

    #[test]
    fn porcelain_parsing_skips_the_header_and_trailer() {
        let stdout = "To git@github.com:o/r.git\n\
                      =\trefs/heads/a:refs/heads/a\t[up to date]\n\
                      Done\n";
        let parsed = parse_porcelain(stdout);
        assert_eq!(parsed.len(), 1, "only the status line is a status line");
        assert_eq!(parsed[0].flag, '=');
    }

    #[test]
    fn a_stale_lease_rejection_is_distinguished_from_an_ordinary_one() {
        let stale = parse_porcelain("!\trefs/heads/a:refs/heads/a\t[rejected] (stale info)");
        assert_eq!(
            result_of(stale[0].clone()),
            Some(PushResult::Rejected {
                detail: "stale info".to_string(),
                stale: true,
            }),
        );

        let includes = parse_porcelain(
            "!\trefs/heads/a:refs/heads/a\t[rejected] (remote ref updated since checkout)",
        );
        assert!(
            matches!(result_of(includes[0].clone()), Some(PushResult::Rejected { stale, .. }) if stale),
            "--force-if-includes wording is a lease refusal too"
        );

        let hook = parse_porcelain(
            "!\trefs/heads/a:refs/heads/a\t[remote rejected] (pre-receive hook declined)",
        );
        assert!(
            matches!(result_of(hook[0].clone()), Some(PushResult::Rejected { stale, .. }) if !stale),
            "a server-side hook refusal is not a lease refusal"
        );
    }

    #[test]
    fn a_rejection_without_a_reason_falls_back_to_the_summary() {
        let parsed = parse_porcelain("!\trefs/heads/a:refs/heads/a\t[rejected]");
        assert_eq!(
            result_of(parsed[0].clone()),
            Some(PushResult::Rejected {
                detail: "[rejected]".to_string(),
                stale: false,
            }),
        );
    }

    #[test]
    fn split_reason_leaves_a_parenthesis_free_summary_alone() {
        assert_eq!(
            split_reason("abc123..def456"),
            ("abc123..def456".to_string(), None)
        );
        assert_eq!(
            split_reason("[rejected] (non-fast-forward)"),
            (
                "[rejected]".to_string(),
                Some("non-fast-forward".to_string())
            )
        );
    }

    // ── classification (git2 reads, real repos) ───────────────────────────

    #[test]
    fn an_unpushed_branch_would_be_created() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");

        let outcome = classify(&wt);
        assert_eq!(outcome.result, PushResult::WouldCreate);
        assert_eq!(
            outcome.remote, "origin",
            "an upstream-less branch falls back to origin"
        );
        assert_eq!(outcome.remote_branch, "feature-a");
    }

    #[test]
    fn a_branch_ahead_of_its_upstream_would_fast_forward() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.publish(&wt, "feature-a");
        scenario.commit_in(&wt, "file.txt", "local\n", "local work");

        assert_eq!(
            classify(&wt).result,
            PushResult::WouldFastForward { ahead: 1 },
            "ahead-only needs no lease"
        );
    }

    #[test]
    fn a_published_branch_with_nothing_new_is_up_to_date() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.publish(&wt, "feature-a");

        assert_eq!(classify(&wt).result, PushResult::UpToDate);
    }

    #[test]
    fn a_rewritten_branch_would_force() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.commit_in(&wt, "file.txt", "worktree first\n", "first");
        scenario.publish(&wt, "feature-a");
        // Rewrite the published tip, exactly as a rebase would.
        scenario.git_in(&wt, &["commit", "--amend", "-m", "rewritten"]);

        assert!(
            matches!(
                classify(&wt).result,
                PushResult::WouldForce {
                    ahead: 1,
                    behind: 1
                }
            ),
            "a rewritten tip diverges and needs the lease"
        );
    }

    #[test]
    fn a_detached_head_is_skipped() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.git_in(&wt, &["checkout", "--detach"]);

        assert_eq!(
            classify(&wt).result,
            PushResult::Skipped {
                reason: SkipReason::DetachedHead
            },
        );
    }

    #[test]
    fn a_non_worktree_path_is_skipped_rather_than_failing_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            classify(dir.path()).result,
            PushResult::Skipped {
                reason: SkipReason::NotAWorktree
            },
        );
    }

    #[test]
    fn a_dirty_worktree_is_not_a_skip() {
        // A push publishes commits, not the working tree — so uncommitted changes
        // are no reason to refuse one (unlike the rebase engine).
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.publish(&wt, "feature-a");
        scenario.commit_in(&wt, "file.txt", "committed\n", "work");
        std::fs::write(wt.join("keep.txt"), "uncommitted\n").unwrap();

        assert_eq!(
            classify(&wt).result,
            PushResult::WouldFastForward { ahead: 1 },
            "a dirty tree must not suppress a push"
        );
    }

    // ── the default-branch gate (ADR-0061 §3) ─────────────────────────────

    #[test]
    fn force_pushing_the_remote_default_branch_is_refused() {
        let _guard = serial();
        let scenario = Scenario::new();
        // Rewrite `main`'s published tip in the main working tree.
        scenario.git_in(&scenario.local, &["commit", "--amend", "-m", "rewritten"]);

        let outcome = classify(&scenario.local);
        assert_eq!(
            outcome.result,
            PushResult::Skipped {
                reason: SkipReason::DefaultBranchForcePush
            },
            "a force-push to the default branch publishes a rewrite to everyone",
        );
    }

    #[test]
    fn fast_forwarding_the_remote_default_branch_stays_allowed() {
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.commit_in(&scenario.local, "file.txt", "more\n", "more");

        assert_eq!(
            classify(&scenario.local).result,
            PushResult::WouldFastForward { ahead: 1 },
            "an ordinary push to the default branch destroys nothing",
        );
    }

    // ── execution against a real bare remote ──────────────────────────────

    #[test]
    fn a_rewritten_branch_is_force_pushed_with_the_lease() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.commit_in(&wt, "file.txt", "worktree first\n", "first");
        scenario.publish(&wt, "feature-a");
        scenario.git_in(&wt, &["commit", "--amend", "-m", "rewritten"]);
        let rewritten = scenario.head_oid(&wt);

        let plan = plan(&Selection::Paths(vec![wt])).unwrap();
        assert!(plan.has_pending_pushes());
        let outcomes = execute(plan, &PushOptions::default());

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].result,
            PushResult::Pushed { forced: true },
            "unexpected outcome: {:?}",
            outcomes[0],
        );
        assert_eq!(
            scenario.origin_oid("refs/heads/feature-a"),
            Some(rewritten),
            "the remote must carry the rewritten tip",
        );
    }

    #[test]
    fn an_unpublished_branch_is_created_with_its_upstream() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.commit_in(&wt, "file.txt", "new\n", "new work");
        let tip = scenario.head_oid(&wt);

        let plan = plan(&Selection::Paths(vec![wt.clone()])).unwrap();
        let outcomes = execute(plan, &PushOptions::default());

        assert_eq!(outcomes[0].result, PushResult::Created);
        assert_eq!(scenario.origin_oid("refs/heads/feature-a"), Some(tip));
        // `--set-upstream` recorded the tracking ref, so a re-plan sees it.
        assert_eq!(classify(&wt).result, PushResult::UpToDate);
    }

    #[test]
    fn a_lease_refusal_is_reported_rather_than_forced_through() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature-a");
        scenario.commit_in(&wt, "file.txt", "worktree first\n", "first");
        scenario.publish(&wt, "feature-a");
        let before = scenario.origin_oid("refs/heads/feature-a");

        // Someone else advances the branch on the server; we never fetch, so our
        // remote-tracking ref — and therefore our lease — still names the old tip.
        scenario.advance_origin("refs/heads/feature-a", "theirs\n");
        let theirs = scenario.origin_oid("refs/heads/feature-a");
        assert_ne!(before, theirs);

        scenario.git_in(&wt, &["commit", "--amend", "-m", "rewritten"]);
        let plan = plan(&Selection::Paths(vec![wt])).unwrap();
        let outcomes = execute(plan, &PushOptions::default());

        match &outcomes[0].result {
            PushResult::Rejected { stale, detail } => assert!(
                *stale,
                "a lease refusal must be flagged so the report can name `git fetch`; got {detail:?}"
            ),
            other => panic!("expected the lease to refuse, got {other:?}"),
        }
        assert_eq!(
            scenario.origin_oid("refs/heads/feature-a"),
            theirs,
            "their commit must survive",
        );
    }

    #[test]
    fn a_batch_continues_past_a_skipped_worktree() {
        let _guard = serial();
        let scenario = Scenario::new();
        let pushable = scenario.add_worktree("feature-a");
        scenario.commit_in(&pushable, "file.txt", "new\n", "new work");
        let detached = scenario.add_worktree("feature-b");
        scenario.git_in(&detached, &["checkout", "--detach"]);

        let plan = plan(&Selection::Paths(vec![detached, pushable])).unwrap();
        let outcomes = execute(plan, &PushOptions::default());

        assert_eq!(outcomes.len(), 2, "selection order is preserved");
        assert_eq!(
            outcomes[0].result,
            PushResult::Skipped {
                reason: SkipReason::DetachedHead
            },
        );
        assert_eq!(outcomes[1].result, PushResult::Created);
    }

    #[test]
    fn all_selects_every_worktree_of_the_repository() {
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        scenario.add_worktree("feature-b");

        let plan = plan(&Selection::All {
            base: scenario.local,
        })
        .unwrap();
        assert_eq!(
            plan.worktrees.len(),
            3,
            "the main working tree is a target like any other (ADR-0060)"
        );
    }

    // ── remote fallback ───────────────────────────────────────────────────

    #[test]
    fn a_repository_with_no_remote_has_nowhere_to_publish() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("solo");
        std::fs::create_dir_all(&local).unwrap();
        git_at(&local, &["init", "-b", "main"]);
        config_repo(&local, "Test", "test@example.com");
        std::fs::write(local.join("file.txt"), "x\n").unwrap();
        git_at(&local, &["add", "file.txt"]);
        git_at(&local, &["commit", "-m", "first"]);

        assert_eq!(
            classify(&local).result,
            PushResult::Skipped {
                reason: SkipReason::NoRemote
            },
        );
    }

    // ── fixture ───────────────────────────────────────────────────────────

    /// A bare `origin` plus a local checkout of `main`, mirroring the rebase
    /// engine's fixture. The bare remote is what makes a *real* push — and a real
    /// lease refusal — testable without a network.
    struct Scenario {
        root: tempfile::TempDir,
        origin: PathBuf,
        local: PathBuf,
    }

    impl Scenario {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let origin = root.path().join("origin.git");
            let local = root.path().join("local");
            std::fs::create_dir_all(&origin).unwrap();
            std::fs::create_dir_all(&local).unwrap();
            git_at(&origin, &["init", "--bare", "-b", "main"]);
            git_at(&local, &["init", "-b", "main"]);
            config_repo(&local, "Test", "test@example.com");
            std::fs::write(local.join("file.txt"), "first\n").unwrap();
            std::fs::write(local.join("keep.txt"), "keep\n").unwrap();
            git_at(&local, &["add", "file.txt", "keep.txt"]);
            git_at(&local, &["commit", "-m", "first"]);
            git_at(
                &local,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            git_at(&local, &["push", "-u", "origin", "main"]);
            Self {
                root,
                origin,
                local,
            }
        }

        /// Adds a linked worktree branched off the current `main`.
        fn add_worktree(&self, name: &str) -> PathBuf {
            let path = self.root.path().join(name);
            git_at(
                &self.local,
                &[
                    "worktree",
                    "add",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            std::fs::canonicalize(&path).unwrap()
        }

        /// Publishes a worktree's branch and records its upstream.
        fn publish(&self, wt: &Path, branch: &str) {
            git_at(wt, &["push", "-u", "origin", branch]);
        }

        /// Commits a change inside a worktree.
        fn commit_in(&self, wt: &Path, file: &str, content: &str, msg: &str) {
            std::fs::write(wt.join(file), content).unwrap();
            git_at(wt, &["add", file]);
            git_at(wt, &["commit", "-m", msg]);
        }

        /// Runs an arbitrary git command in a worktree.
        fn git_in(&self, wt: &Path, args: &[&str]) {
            git_at(wt, args);
        }

        /// Advances a ref on the **server** with `git2`, writing straight into the
        /// bare origin's object database — the local repo only learns of it on a
        /// fetch it never performs, which is exactly the lease scenario.
        fn advance_origin(&self, refname: &str, content: &str) {
            let repo = Repository::open_bare(&self.origin).unwrap();
            let parent = repo
                .find_commit(repo.refname_to_id(refname).unwrap())
                .unwrap();
            let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
            let blob = repo.blob(content.as_bytes()).unwrap();
            builder.insert("file.txt", blob, 0o100_644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::now("Other", "other@example.com").unwrap();
            repo.commit(Some(refname), &sig, &sig, "theirs", &tree, &[&parent])
                .unwrap();
        }

        /// A ref's tip on the server, or `None` when it does not exist there.
        fn origin_oid(&self, refname: &str) -> Option<Oid> {
            Repository::open_bare(&self.origin)
                .unwrap()
                .refname_to_id(refname)
                .ok()
        }

        /// A worktree's current HEAD commit.
        fn head_oid(&self, wt: &Path) -> Oid {
            Repository::open(wt)
                .unwrap()
                .head()
                .unwrap()
                .target()
                .unwrap()
        }
    }

    fn config_repo(dir: &Path, name: &str, email: &str) {
        git_at(dir, &["config", "user.name", name]);
        git_at(dir, &["config", "user.email", email]);
        git_at(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn git_at(dir: &Path, args: &[&str]) {
        let output = run_git_in(&resolve_git_binary(), dir, args).unwrap();
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
