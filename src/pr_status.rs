//! GitHub PR check-badge resolution for the worktrees tree (#1337).
//!
//! The engine half of the PR badge, mirroring the [`crate::worktrees`] registry /
//! [`crate::daemon::services::worktrees`] adapter split: this module is pure
//! resolution — build a query, run `gh`, reduce the reply, cache the result — and
//! knows nothing about the daemon, the socket, or the tray. The adapter owns the
//! poll loop and the change-notify.
//!
//! # Why this lives in the daemon
//!
//! Badges were resolved extension-side ([ADR-0050]), which made cost scale with
//! the number of open VS Code windows and — the actual bug — meant nothing ever
//! re-asked GitHub after a badge was first computed. Resolving once in the daemon
//! and fanning the answer out over the existing subscribe stream makes cost
//! invariant to window count and lets an unfocused window show live CI state.
//!
//! # No credential enters the daemon
//!
//! Resolution shells out to `gh api graphql`, so the GitHub token stays inside
//! `gh` where the user already put it — exactly as ADR-0050 wanted, and following
//! ADR-0003's "shell out to `gh`/`git` for GitHub operations". This is why moving
//! resolution daemon-side does **not** widen the worktrees service's no-credential
//! posture (ADR-0040): the daemon never sees a token.
//!
//! # Why one query for everything
//!
//! `repository(owner:,name:)` and `ref(qualifiedName:)` take no `first:`/`last:`
//! argument, so neither is a GraphQL *connection* and neither contributes to the
//! query's cost. Aliasing them is free: one query covering every (repo, branch)
//! pair the tree knows about costs **1 point** against the 5,000/hour budget,
//! independent of how many repos, worktrees, or windows are open. Measured against
//! `rust-works/omni-dev`: cost 1 up to ~50 branches, 2 at 100, 4 at 200.
//!
//! Two shapes are deliberately avoided:
//!
//! - `gh pr list --json statusCheckRollup`, which the extension used, is an alias
//!   for a `commits(last:1)` connection — it adds one request per PR and costs 2.
//! - `checkSuites { checkRuns { totalCount } }` is a third-level connection and
//!   costs **11**.
//!
//! [ADR-0050]: ../../docs/adrs/adr-0050.md

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

/// Environment override for the `gh` binary, for when the daemon runs under
/// launchd/systemd with a minimal `PATH` that does not contain it. Mirrors
/// `OMNI_DEV_VSCODE_BIN` for the tray's `code` launcher, and the companion
/// extension's override of the same name (`editors/vscode/src/gh.ts`).
const GH_BIN_ENV: &str = "OMNI_DEV_GH_BIN";

/// Absolute paths probed for `gh` when [`GH_BIN_ENV`] is unset, in order. The
/// daemon cannot rely on `PATH`: launchd hands it a minimal one.
const GH_BINARY_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/gh",
    "/usr/local/bin/gh",
    "/home/linuxbrew/.linuxbrew/bin/gh",
    "/usr/bin/gh",
];

/// GitHub check conclusions / status-context states that mean **failed**. Values
/// are upper-cased before lookup, so these are the canonical GraphQL enum names.
const FAILURE_STATES: &[&str] = &[
    "FAILURE",
    "ERROR",
    "CANCELLED",
    "TIMED_OUT",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "STALE",
];

/// States that count as **passing**. A skipped/neutral check is non-blocking, so
/// it passes — matching how `gh pr checks` treats "skipping".
const SUCCESS_STATES: &[&str] = &["SUCCESS", "NEUTRAL", "SKIPPED"];

/// The rolled-up CI verdict for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrCheckState {
    /// Every reported check passed (or was skipped/neutral).
    Success,
    /// At least one check failed — failure dominates every other state.
    Failure,
    /// At least one check is still running, or reported a state we do not
    /// recognise. Never a false pass.
    Pending,
    /// No checks reported at all. Renders no badge.
    None,
}

/// The PR badge shown on a worktree row: which PR heads this branch and how its
/// CI is doing.
///
/// `isDraft` is **camelCase on the wire** — the extension's `PrBadge` inherited
/// the name from `gh`'s JSON output before badges moved daemon-side, and every
/// consumer already reads that key. Renaming it to match the payload's otherwise
/// snake_case convention would silently drop the draft marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrBadge {
    /// The PR number, e.g. `1337`.
    pub number: u64,
    /// Whether the PR is a draft.
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    /// The rolled-up CI verdict.
    pub checks: PrCheckState,
    /// The PR's web URL, for the open action.
    pub url: String,
}

/// One (repo, branch) pair to resolve a badge for. Derived from the tree — a repo
/// contributes a target per worktree that has a branch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrTarget {
    /// The GitHub owner, e.g. `rust-works`.
    pub owner: String,
    /// The GitHub repo name, e.g. `omni-dev`.
    pub name: String,
    /// The branch checked out in the worktree.
    pub branch: String,
}

/// How a **single** rollup entry classifies.
///
/// Deliberately narrower than [`PrCheckState`]: an individual check is always
/// failing, running, or passing. "No checks" is a property of the rollup as a
/// whole, never of an entry — so rather than carry an impossible `None` case as an
/// unreachable match arm (as the TypeScript this was ported from did), it simply is
/// not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Failure,
    Pending,
    Success,
}

/// Classifies one `statusCheckRollup` entry.
///
/// A `CheckRun` that has not `COMPLETED` is pending regardless of its (null)
/// conclusion; otherwise the `conclusion` (a `CheckRun`) or `state` (a legacy
/// `StatusContext`) decides. Anything unrecognised is pending, so a still-resolving
/// or unknown check never reads as passing.
fn check_entry_state(entry: &Value) -> EntryState {
    let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
    if !status.is_empty() && !status.eq_ignore_ascii_case("COMPLETED") {
        return EntryState::Pending;
    }
    // `conclusion` then `state`, each ignored when empty — a completed CheckRun
    // carries `conclusion`, a StatusContext carries `state`, and neither is set on
    // an entry still resolving.
    let raw = entry
        .get("conclusion")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            entry
                .get("state")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_ascii_uppercase();
    if FAILURE_STATES.contains(&raw.as_str()) {
        return EntryState::Failure;
    }
    if SUCCESS_STATES.contains(&raw.as_str()) {
        return EntryState::Success;
    }
    EntryState::Pending
}

/// Reduces a PR's rollup contexts to one verdict: any failing check dominates
/// (`failure`); else any still-running one (`pending`); else `success`. An empty
/// rollup means no checks (`none`) — the only way `none` arises, since every entry
/// classifies as one of the three.
fn rollup_check_state(contexts: &[Value]) -> PrCheckState {
    if contexts.is_empty() {
        return PrCheckState::None;
    }
    let mut saw_pending = false;
    for entry in contexts {
        match check_entry_state(entry) {
            EntryState::Failure => return PrCheckState::Failure,
            EntryState::Pending => saw_pending = true,
            EntryState::Success => {}
        }
    }
    // Non-empty, nothing failing, nothing running ⇒ everything passed. (The
    // TypeScript tracked a `sawSuccess` flag here; it could never be false at this
    // point, so the branch it guarded was dead.)
    if saw_pending {
        PrCheckState::Pending
    } else {
        PrCheckState::Success
    }
}

/// The GraphQL fragment resolving one branch: its head OID, the rollup contexts
/// the verdict is reduced from, and the open PR that heads it.
///
/// `associatedPullRequests` is read off the **`Ref`**, filtered to `OPEN`, not off
/// the `Commit`. On a `Commit` it returns whatever PR introduced that commit — on
/// `main` that is the last *merged* PR, from an unrelated branch, which would paint
/// a false badge. On a `Ref` it matches on **head**, reproducing the extension's
/// "first open PR whose `headRefName` is this branch" rule (verified: `main` is the
/// base of many open PRs and correctly resolves to nothing).
fn branch_fragment(alias: &str, branch: &str) -> String {
    // JSON string escaping is valid GraphQL string escaping, so this is safe for
    // any branch name git permits.
    let qualified = Value::String(format!("refs/heads/{branch}"));
    format!(
        r"{alias}: ref(qualifiedName:{qualified}){{
      target{{ ...on Commit{{ oid
        statusCheckRollup{{ contexts(first:100){{ nodes{{
          __typename
          ...on CheckRun{{ status conclusion }}
          ...on StatusContext{{ state }}
        }} }} }}
      }} }}
      associatedPullRequests(first:1, states:OPEN){{ nodes{{ number isDraft url }} }}
    }}"
    )
}

/// Maps a query's `(repo alias index, branch alias index)` back to the target it
/// was built for, so the reply can be read without re-deriving the aliasing.
type QueryIndex = HashMap<(usize, usize), PrTarget>;

/// Builds the single aliased query for every target, plus the alias→target index
/// needed to read the reply back. Targets are grouped by repo so each repo appears
/// once. Returns `None` when there is nothing to ask.
fn build_query(targets: &[PrTarget]) -> Option<(String, QueryIndex)> {
    if targets.is_empty() {
        return None;
    }
    // BTreeMap so aliases are assigned deterministically — the query text is then
    // stable for a stable target set, which keeps it testable.
    let mut by_repo: BTreeMap<(&str, &str), Vec<&PrTarget>> = BTreeMap::new();
    for t in targets {
        by_repo
            .entry((t.owner.as_str(), t.name.as_str()))
            .or_default()
            .push(t);
    }
    let mut index = HashMap::new();
    let mut repos = Vec::new();
    for (ri, ((owner, name), branches)) in by_repo.iter().enumerate() {
        let mut frags = Vec::new();
        for (bi, target) in branches.iter().enumerate() {
            frags.push(branch_fragment(&format!("b{bi}"), &target.branch));
            index.insert((ri, bi), (*target).clone());
        }
        let owner = Value::String((*owner).to_string());
        let name = Value::String((*name).to_string());
        repos.push(format!(
            "r{ri}: repository(owner:{owner}, name:{name}){{\n{}\n}}",
            frags.join("\n")
        ));
    }
    Some((format!("query{{\n{}\n}}", repos.join("\n")), index))
}

/// Reads one resolved `ref` node into a badge. `None` — rendering no badge — for a
/// branch that does not exist on the remote (never pushed), or that no open PR
/// heads.
fn badge_from_ref(node: &Value) -> Option<PrBadge> {
    let pr = node
        .get("associatedPullRequests")?
        .get("nodes")?
        .as_array()?
        .first()?;
    let contexts = node
        .get("target")
        .and_then(|t| t.get("statusCheckRollup"))
        .and_then(|r| r.get("contexts"))
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some(PrBadge {
        number: pr.get("number").and_then(Value::as_u64)?,
        is_draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        checks: rollup_check_state(&contexts),
        url: pr
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Reads the GraphQL reply back into badges, keyed by target.
///
/// Best-effort per branch: a missing or malformed node is skipped rather than
/// sinking the whole poll, matching the tree's "absent field → absent indicator"
/// degradation.
fn parse_response(body: &Value, index: &QueryIndex) -> HashMap<PrTarget, PrBadge> {
    let mut out = HashMap::new();
    let Some(data) = body.get("data") else {
        return out;
    };
    for ((ri, bi), target) in index {
        let node = data
            .get(format!("r{ri}"))
            .and_then(|r| r.get(format!("b{bi}")));
        // A null ref (unpushed branch) is expected, not an error.
        let Some(node) = node.filter(|n| !n.is_null()) else {
            continue;
        };
        if let Some(badge) = badge_from_ref(node) {
            out.insert(target.clone(), badge);
        }
    }
    out
}

/// Resolves `gh`, preferring [`GH_BIN_ENV`], then the first existing well-known
/// absolute path, then bare `gh` on `PATH`.
///
/// Callers should do this **once** (the poller resolves it at spawn) and pass the
/// result to [`resolve_with`], rather than re-reading the environment per poll.
#[must_use]
pub fn resolve_gh_binary() -> PathBuf {
    resolve_gh_binary_from(std::env::var_os(GH_BIN_ENV), GH_BINARY_CANDIDATES)
}

/// The testable core of [`resolve_gh_binary`]. Split so the probe order can be
/// unit-tested without mutating the process environment.
fn resolve_gh_binary_from(
    env_override: Option<std::ffi::OsString>,
    candidates: &[&str],
) -> PathBuf {
    if let Some(path) = env_override.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return path.to_path_buf();
        }
    }
    PathBuf::from("gh")
}

/// Runs one `gh api graphql` call against `bin`. **Blocking** — callers must be on
/// a blocking thread, never an async worker.
fn run_gh_graphql(bin: &Path, query: &str) -> Result<Value> {
    let output = Command::new(bin)
        .args(["api", "graphql", "-f"])
        .arg(format!("query={query}"))
        .output()
        .with_context(|| {
            format!(
                "failed to run {} (is the GitHub CLI installed?)",
                bin.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api graphql failed: {}", stderr.trim());
    }
    serde_json::from_slice(&output.stdout).context("gh api graphql returned invalid JSON")
}

/// Resolves a badge for every target in **one** `gh api graphql` call, using the
/// `gh` at `bin`.
///
/// The binary is a parameter rather than resolved here so callers read the
/// environment **once** (the poller does it at spawn, like its interval) and so
/// tests inject a stub without mutating the process environment — two parallel
/// tests pointing one global env var at different fakes race, and the project is
/// migrating away from test env locks toward injection (#1030).
///
/// **Blocking** — run on a blocking thread. Returns only the targets that resolved
/// to an open PR; a branch with no PR, or not pushed, is simply absent.
pub fn resolve_with(bin: &Path, targets: &[PrTarget]) -> Result<HashMap<PrTarget, PrBadge>> {
    let Some((query, index)) = build_query(targets) else {
        return Ok(HashMap::new());
    };
    let body = run_gh_graphql(bin, &query)?;
    // A GraphQL 200 can still carry errors; surface them rather than silently
    // reporting "no badges", which would look identical to "no PRs".
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            bail!("gh api graphql returned errors: {errors:?}");
        }
    }
    Ok(parse_response(&body, &index))
}

/// The poller-written, snapshot-read badge cache.
///
/// A plain `std::Mutex` map: writes come from the poll loop, reads from the tree
/// snapshot build. The lock is never held across an `.await` — every method takes
/// it, finishes, and drops it.
#[derive(Debug, Default)]
pub struct PrStatusCache {
    badges: Mutex<HashMap<PrTarget, PrBadge>>,
}

impl PrStatusCache {
    /// An empty cache. Until the first poll lands, every lookup misses and the
    /// tree simply renders no badge.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The badge for one (repo, branch), if resolved.
    #[must_use]
    pub fn get(&self, owner: &str, name: &str, branch: &str) -> Option<PrBadge> {
        let key = PrTarget {
            owner: owner.to_string(),
            name: name.to_string(),
            branch: branch.to_string(),
        };
        self.lock().get(&key).cloned()
    }

    /// Replaces the cache wholesale, returning whether anything actually changed.
    ///
    /// The bool is load-bearing: the caller bumps the registry's change-notify only
    /// when it is `true`. Bumping unconditionally would defeat the server's
    /// diff-and-drop and re-push an identical snapshot to every window on every
    /// poll — the cost this whole design exists to avoid.
    pub fn replace(&self, next: HashMap<PrTarget, PrBadge>) -> bool {
        let mut guard = self.lock();
        if *guard == next {
            return false;
        }
        *guard = next;
        true
    }

    /// Whether any cached badge is still pending — the poller's cadence signal.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.lock()
            .values()
            .any(|b| b.checks == PrCheckState::Pending)
    }

    /// Poison-tolerant lock: a panicking holder must not wedge the badge cache,
    /// which is best-effort decoration.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PrTarget, PrBadge>> {
        self.badges.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(branch: &str) -> PrTarget {
        PrTarget {
            owner: "rust-works".into(),
            name: "omni-dev".into(),
            branch: branch.into(),
        }
    }

    // --- Reducer: ported verbatim from the extension's github.test.ts so the
    //     daemon-side move is behaviour-preserving (#1337 PR 2). ---

    #[test]
    fn rollup_is_none_for_an_empty_rollup() {
        assert_eq!(rollup_check_state(&[]), PrCheckState::None);
    }

    #[test]
    fn rollup_reads_completed_check_run_conclusions() {
        for (conclusion, want) in [
            ("SUCCESS", PrCheckState::Success),
            ("NEUTRAL", PrCheckState::Success),
            ("SKIPPED", PrCheckState::Success),
            ("FAILURE", PrCheckState::Failure),
            ("CANCELLED", PrCheckState::Failure),
            ("TIMED_OUT", PrCheckState::Failure),
            ("ACTION_REQUIRED", PrCheckState::Failure),
            ("STARTUP_FAILURE", PrCheckState::Failure),
            ("STALE", PrCheckState::Failure),
        ] {
            let entry =
                json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":conclusion});
            assert_eq!(
                rollup_check_state(&[entry]),
                want,
                "conclusion {conclusion} should be {want:?}"
            );
        }
    }

    #[test]
    fn rollup_treats_an_incomplete_check_run_as_pending() {
        // The conclusion is null while running; the status decides.
        for status in ["IN_PROGRESS", "QUEUED", "WAITING", "PENDING"] {
            let entry = json!({"__typename":"CheckRun","status":status,"conclusion":null});
            assert_eq!(rollup_check_state(&[entry]), PrCheckState::Pending);
        }
    }

    #[test]
    fn rollup_reads_status_context_states() {
        // A legacy StatusContext has no `status`, so `state` decides.
        for (state, want) in [
            ("SUCCESS", PrCheckState::Success),
            ("FAILURE", PrCheckState::Failure),
            ("ERROR", PrCheckState::Failure),
            ("PENDING", PrCheckState::Pending),
            ("EXPECTED", PrCheckState::Pending),
        ] {
            let entry = json!({"__typename":"StatusContext","state":state});
            assert_eq!(rollup_check_state(&[entry]), want, "state {state}");
        }
    }

    #[test]
    fn rollup_never_reads_an_unknown_value_as_a_pass() {
        // The load-bearing rule: anything unrecognised is pending, never success.
        let entry = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"WAT"});
        assert_eq!(rollup_check_state(&[entry]), PrCheckState::Pending);
        // A completed-but-unset conclusion likewise.
        let entry = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":""});
        assert_eq!(rollup_check_state(&[entry]), PrCheckState::Pending);
    }

    #[test]
    fn rollup_precedence_is_failure_then_pending_then_success() {
        let ok = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"});
        let bad = json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"});
        let run = json!({"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null});
        // Failure dominates everything, in either order.
        assert_eq!(
            rollup_check_state(&[ok.clone(), run.clone(), bad.clone()]),
            PrCheckState::Failure
        );
        assert_eq!(
            rollup_check_state(&[bad, ok.clone()]),
            PrCheckState::Failure
        );
        // Pending beats success.
        assert_eq!(
            rollup_check_state(&[ok.clone(), run]),
            PrCheckState::Pending
        );
        assert_eq!(rollup_check_state(&[ok]), PrCheckState::Success);
    }

    // --- Query shape ---

    #[test]
    fn build_query_is_none_without_targets() {
        assert!(build_query(&[]).is_none());
    }

    #[test]
    fn build_query_groups_branches_under_one_repo_alias() {
        let (query, index) = build_query(&[target("main"), target("feature")]).unwrap();
        // One repo → one `repository(...)` block, two `ref(...)` aliases.
        assert_eq!(query.matches("repository(owner:").count(), 1);
        assert_eq!(query.matches(": ref(qualifiedName:").count(), 2);
        assert!(query.contains(r#""refs/heads/main""#), "{query}");
        assert!(query.contains(r#""refs/heads/feature""#), "{query}");
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn build_query_reads_open_prs_off_the_ref_not_the_commit() {
        // Guards the semantic trap: `Commit.associatedPullRequests` returns the PR
        // that *introduced* the commit — on `main`, the last merged PR from an
        // unrelated branch — which would paint a false badge. It must be read off
        // the Ref, filtered to OPEN, which matches on head.
        let (query, _) = build_query(&[target("main")]).unwrap();
        assert!(
            query.contains("associatedPullRequests(first:1, states:OPEN)"),
            "{query}"
        );
        // The PR lookup must sit outside the `target{...on Commit{...}}` block.
        let commit_block = query.find("...on Commit").unwrap();
        let pr_lookup = query.find("associatedPullRequests").unwrap();
        assert!(
            pr_lookup > commit_block,
            "PR lookup must be on the Ref, after the Commit block"
        );
    }

    #[test]
    fn build_query_separates_distinct_repos() {
        let a = PrTarget {
            owner: "o1".into(),
            name: "r1".into(),
            branch: "main".into(),
        };
        let b = PrTarget {
            owner: "o2".into(),
            name: "r2".into(),
            branch: "main".into(),
        };
        let (query, index) = build_query(&[a, b]).unwrap();
        assert_eq!(query.matches("repository(owner:").count(), 2);
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn build_query_escapes_branch_names() {
        // Defensive: JSON escaping keeps a quote in a ref name from breaking out of
        // the GraphQL string literal.
        let (query, _) = build_query(&[target(r#"we"ird"#)]).unwrap();
        assert!(query.contains(r#"refs/heads/we\"ird"#), "{query}");
    }

    // --- Reply parsing ---

    #[test]
    fn parse_response_reads_badges_and_skips_absent_refs() {
        let targets = vec![target("feature"), target("unpushed"), target("no-pr")];
        let (_, index) = build_query(&targets).unwrap();
        // Aliases are assigned in BTreeMap order of (owner,name) then input order.
        let body = json!({"data":{"r0":{
            "b0": {
                "target": {"oid":"abc","statusCheckRollup":{"contexts":{"nodes":[
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":65,"isDraft":true,"url":"u"}]}
            },
            // An unpushed branch resolves to a null ref — expected, not an error.
            "b1": null,
            // Pushed, but no open PR heads it.
            "b2": {
                "target": {"oid":"def","statusCheckRollup":null},
                "associatedPullRequests":{"nodes":[]}
            }
        }}});
        let out = parse_response(&body, &index);
        assert_eq!(out.len(), 1, "{out:?}");
        let badge = out.get(&target("feature")).unwrap();
        assert_eq!(badge.number, 65);
        assert!(badge.is_draft);
        assert_eq!(badge.checks, PrCheckState::Success);
        assert_eq!(badge.url, "u");
    }

    #[test]
    fn parse_response_reads_a_pr_with_no_checks_as_none() {
        let targets = vec![target("feature")];
        let (_, index) = build_query(&targets).unwrap();
        let body = json!({"data":{"r0":{"b0":{
            "target": {"oid":"abc","statusCheckRollup":null},
            "associatedPullRequests":{"nodes":[{"number":7,"isDraft":false,"url":"u"}]}
        }}}});
        let out = parse_response(&body, &index);
        assert_eq!(
            out.get(&target("feature")).unwrap().checks,
            PrCheckState::None
        );
    }

    #[test]
    fn parse_response_tolerates_a_missing_data_block() {
        let (_, index) = build_query(&[target("x")]).unwrap();
        assert!(parse_response(&json!({}), &index).is_empty());
    }

    // --- Wire shape ---

    #[test]
    fn badge_serializes_is_draft_as_camel_case() {
        // The extension's PrBadge inherited `isDraft` from gh's JSON. Serializing it
        // as `is_draft` would silently drop the draft marker on every row.
        let badge = PrBadge {
            number: 65,
            is_draft: true,
            checks: PrCheckState::Pending,
            url: "u".into(),
        };
        let v = serde_json::to_value(&badge).unwrap();
        assert_eq!(v["isDraft"], json!(true));
        assert_eq!(v["checks"], json!("pending"));
        assert!(v.get("is_draft").is_none(), "{v}");
    }

    #[test]
    fn check_state_serializes_lowercase() {
        for (state, want) in [
            (PrCheckState::Success, "success"),
            (PrCheckState::Failure, "failure"),
            (PrCheckState::Pending, "pending"),
            (PrCheckState::None, "none"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(want));
        }
    }

    // --- Binary resolution ---

    #[test]
    fn resolve_gh_binary_from_prefers_env_then_candidate_then_fallback() {
        assert_eq!(
            resolve_gh_binary_from(Some("/custom/gh".into()), &["/usr/bin/gh"]),
            PathBuf::from("/custom/gh")
        );
        let existing = tempfile::NamedTempFile::new().unwrap();
        let existing_path = existing.path().to_str().unwrap();
        assert_eq!(
            resolve_gh_binary_from(None, &["/no/such/gh/xyzzy", existing_path]),
            PathBuf::from(existing_path)
        );
        assert_eq!(
            resolve_gh_binary_from(None, &["/no/such/gh/xyzzy"]),
            PathBuf::from("gh")
        );
        // An empty override falls through rather than resolving to "".
        assert_eq!(
            resolve_gh_binary_from(Some("".into()), &["/no/such/gh/xyzzy"]),
            PathBuf::from("gh")
        );
        // The real-env wrapper resolves without panicking.
        let _ = resolve_gh_binary();
    }

    // --- resolve_with: the degradation contract ---
    //
    // Badges are decoration: a missing, unauthenticated, or failing `gh` must
    // surface an error to the poller (which backs off and keeps the last good
    // badges) and never panic or hang. These cover the paths that decide that.

    /// Writes an executable stub standing in for `gh`, printing `stdout` and
    /// exiting `code`.
    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> PathBuf {
        let path = dir.join("fake-gh");
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n"),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn resolve_with_asks_nothing_for_no_targets() {
        // No branches to resolve → no subprocess at all. The binary is deliberately
        // bogus: if this spawned anything it would error instead of returning empty.
        let out = resolve_with(Path::new("/no/such/gh/xyzzy"), &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn resolve_with_errors_when_gh_is_missing() {
        // The common real case: `gh` not installed, or absent from launchd's
        // minimal PATH.
        let err = resolve_with(Path::new("/no/such/gh/xyzzy"), &[target("main")]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to run"), "{msg}");
        assert!(msg.contains("GitHub CLI"), "{msg}");
    }

    #[test]
    fn resolve_with_errors_on_a_nonzero_exit() {
        // e.g. `gh auth login` never run: gh exits non-zero and explains on stderr.
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path(), "", 1);
        let err = resolve_with(&bin, &[target("main")]).unwrap_err();
        assert!(
            format!("{err:#}").contains("gh api graphql failed"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_with_errors_on_unparseable_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path(), "not json at all", 0);
        let err = resolve_with(&bin, &[target("main")]).unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"), "{err:#}");
    }

    #[test]
    fn resolve_with_surfaces_graphql_errors_rather_than_reporting_no_badges() {
        // A GraphQL 200 can still carry errors (a bad field, a rate limit). Reading
        // that as an empty result would be indistinguishable from "no open PRs" and
        // would silently blank every badge, so it must be an error.
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(
            dir.path(),
            r#"{"data":null,"errors":[{"message":"API rate limit exceeded"}]}"#,
            0,
        );
        let err = resolve_with(&bin, &[target("main")]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("returned errors"), "{msg}");
        assert!(msg.contains("rate limit"), "{msg}");
    }

    #[test]
    fn resolve_with_ignores_an_empty_errors_array() {
        // Some responses carry `errors: []`; that is a success, not a failure.
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(
            dir.path(),
            r#"{"errors":[],"data":{"r0":{"b0":{
                "target":{"oid":"a","statusCheckRollup":null},
                "associatedPullRequests":{"nodes":[{"number":9,"isDraft":false,"url":"u"}]}}}}}"#,
            0,
        );
        let out = resolve_with(&bin, &[target("main")]).unwrap();
        assert_eq!(out.get(&target("main")).unwrap().number, 9);
    }

    #[test]
    fn resolve_with_reads_a_real_reply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(
            dir.path(),
            r#"{"data":{"r0":{"b0":{
                "target":{"oid":"a","statusCheckRollup":{"contexts":{"nodes":[
                  {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":42,"isDraft":true,"url":"u42"}]}}}}}"#,
            0,
        );
        let out = resolve_with(&bin, &[target("main")]).unwrap();
        let badge = out.get(&target("main")).unwrap();
        assert_eq!(badge.number, 42);
        assert!(badge.is_draft);
        assert_eq!(badge.checks, PrCheckState::Failure);
    }

    // --- Cache ---

    #[test]
    fn cache_replace_reports_whether_anything_changed() {
        let cache = PrStatusCache::new();
        let badge = PrBadge {
            number: 1,
            is_draft: false,
            checks: PrCheckState::Pending,
            url: "u".into(),
        };
        let mut map = HashMap::new();
        map.insert(target("f"), badge);

        // First write is a change.
        assert!(cache.replace(map.clone()));
        // An identical write is not — this is what keeps the poller from re-pushing
        // an unchanged snapshot to every window on every tick.
        assert!(!cache.replace(map.clone()));

        // A changed verdict is a change.
        let mut moved = map.clone();
        moved.get_mut(&target("f")).unwrap().checks = PrCheckState::Success;
        assert!(cache.replace(moved));
        // Emptying is a change.
        assert!(cache.replace(HashMap::new()));
        assert!(!cache.replace(HashMap::new()));
    }

    #[test]
    fn cache_get_and_any_pending() {
        let cache = PrStatusCache::new();
        assert!(cache.get("rust-works", "omni-dev", "f").is_none());
        assert!(!cache.any_pending());

        let mut map = HashMap::new();
        map.insert(
            target("f"),
            PrBadge {
                number: 1,
                is_draft: false,
                checks: PrCheckState::Pending,
                url: "u".into(),
            },
        );
        cache.replace(map.clone());
        assert_eq!(cache.get("rust-works", "omni-dev", "f").unwrap().number, 1);
        assert!(cache.any_pending());
        // A miss on a branch we never resolved.
        assert!(cache.get("rust-works", "omni-dev", "other").is_none());

        map.get_mut(&target("f")).unwrap().checks = PrCheckState::Success;
        cache.replace(map);
        assert!(!cache.any_pending());
    }
}
