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
//! # The explicit negative (#1370)
//!
//! Resolution is tri-state per target, not binary. "No open PR heads this branch"
//! is a *successful* answer ([`PrResolution::NoPr`]), distinct from "never
//! resolved" (absent from the cache). Absence used to carry both meanings, which
//! left a client unable to tell "checked, none" from "old daemon that never
//! checked" — so every window's degraded `gh pr list` fallback re-fired per
//! window × repo × 60s forever, burning the very GraphQL budget this module
//! exists to protect. A **failed** poll still reports nothing: negatives are only
//! ever minted from a successfully parsed reply.
//!
//! [ADR-0050]: ../../docs/adrs/adr-0050.md

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

/// Reconstructs badges from #1378 webhook-buffer deltas, reusing this module's
/// private [`rollup_check_state`] reducer (a child module may read its parent's
/// private items) so a webhook verdict equals the verdict the GraphQL poll would
/// produce. Driven by the daemon's `WebhookPrSource`.
pub(crate) mod webhook;

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
    /// The commit on the remote branch that [`checks`](Self::checks) describes.
    ///
    /// **Not on the wire** — it exists so the snapshot fold can tell a verdict
    /// computed for *some* commit from one computed for the commit the worktree
    /// actually has checked out. Without it a badge cannot know it is out of date,
    /// and the previous head's verdict stands until the next poll (#1337). See
    /// [`is_stale_for`](Self::is_stale_for).
    #[serde(skip)]
    pub head_oid: String,
}

impl PrBadge {
    /// Whether this verdict describes a commit other than `head_sha`.
    ///
    /// The check is deliberately "different", not "older": we cannot order two
    /// commits without a walk, and every way they can differ means the same thing —
    /// **this verdict is not about the commit in front of you**. You pushed and CI
    /// has not reported yet; you have unpushed work; you are behind the remote.
    ///
    /// This is what makes a push invalidate the badge *immediately*, with no network
    /// call: the cache still holds the previous commit's oid, so the mismatch is
    /// visible on the very next snapshot. A `None` head (an unborn HEAD) is not
    /// stale — there is nothing to compare, and such a worktree has no branch and so
    /// no badge anyway.
    #[must_use]
    pub fn is_stale_for(&self, head_sha: Option<&str>) -> bool {
        head_sha.is_some_and(|sha| sha != self.head_oid)
    }
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

/// The outcome for one **successfully checked** target (#1370).
///
/// [`NoPr`](Self::NoPr) deliberately carries no `head_oid`: a negative has no
/// verdict to be stale against, and carrying the remote head would turn every
/// push into a `NoPr(a) → NoPr(b)` map change — spuriously bumping the
/// change-notify that [`PrStatusCache::replace`] exists to gate. A negative that
/// *is* out of date (a PR was just opened) is corrected by the poller's
/// `moved`-triggered immediate re-poll instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrResolution {
    /// An open PR heads the branch; here is its badge.
    Pr(PrBadge),
    /// The branch was checked against GitHub and **no open PR** heads it —
    /// including a branch never pushed. Serialized as `pr_none: true` on the
    /// tree wire, so a client can tell "checked, none" from "not resolved" and
    /// keep its degraded fallback quiet.
    NoPr,
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
    if saw_pending {
        return PrCheckState::Pending;
    }
    // Every check *that exists* has passed — but more may still be coming, so this
    // is not yet a pass. See [`suite_still_running`].
    if contexts.iter().any(suite_still_running) {
        return PrCheckState::Pending;
    }
    // Non-empty, nothing failing, nothing running, every suite terminal ⇒ passed.
    // (The TypeScript tracked a `sawSuccess` flag here; it could never be false at
    // this point, so the branch it guarded was dead.)
    PrCheckState::Success
}

/// Whether the check **suite** backing this entry is still running.
///
/// This is what catches the `needs:`-gate false green. GitHub does not create a
/// gated job's check run until its dependency completes, so in the window between
/// the gate passing and the fan-out appearing, every check run that *exists* is
/// green and the rollup reduces to `success` — GitHub's own aggregate `state` says
/// `SUCCESS` too. Only the suite knows more jobs are coming.
///
/// Reading the suite off a **`CheckRun` in the rollup** (rather than querying
/// `checkSuites` on the commit) is what makes this safe as well as free. A suite
/// only appears here by way of a check run it owns, so a suite with **zero** runs —
/// e.g. codecov leaves one `QUEUED` on every PR, observed still queued after 3.7
/// days — is never seen and cannot pin a badge yellow forever. No age heuristic, no
/// app denylist, and no third-level `checkRuns { totalCount }` connection (which
/// would cost 11 points instead of 1).
fn suite_still_running(entry: &Value) -> bool {
    entry
        .get("checkSuite")
        .and_then(|suite| suite.get("status"))
        .and_then(Value::as_str)
        .is_some_and(|status| !status.is_empty() && !status.eq_ignore_ascii_case("COMPLETED"))
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
fn branch_fragment(alias: &str, branch: &str, include_rollup: bool) -> String {
    // JSON string escaping is valid GraphQL string escaping, so this is safe for
    // any branch name git permits.
    let qualified = Value::String(format!("refs/heads/{branch}"));
    // `statusCheckRollup.contexts(first:100)` is the query's cost driver — one
    // connection of up to 100 nodes **per ref**. The webhook source's metadata-only
    // reconcile (#1384) omits it for webhook-backed repos, whose CI verdict comes
    // from webhook events, not GraphQL — keeping only `oid` (for staleness) and the
    // open-PR metadata, which are one node each. With `include_rollup = true` the
    // fragment is byte-identical to the original full query.
    let rollup = if include_rollup {
        r"
        statusCheckRollup{ contexts(first:100){ nodes{
          __typename
          ...on CheckRun{ status conclusion checkSuite{ status } }
          ...on StatusContext{ state }
        } } }"
    } else {
        ""
    };
    format!(
        r"{alias}: ref(qualifiedName:{qualified}){{
      target{{ ...on Commit{{ oid{rollup}
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
    build_query_inner(targets, true)
}

/// [`build_query`] without the expensive `statusCheckRollup` — the webhook source's
/// metadata-only reconcile (#1384). The reply parses through the same path; a ref
/// with no rollup reads as `checks: None` ([`badge_from_ref`] defaults the empty
/// context list), and the CI verdict is supplied by webhook events instead.
fn build_metadata_query(targets: &[PrTarget]) -> Option<(String, QueryIndex)> {
    build_query_inner(targets, false)
}

fn build_query_inner(targets: &[PrTarget], include_rollup: bool) -> Option<(String, QueryIndex)> {
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
            frags.push(branch_fragment(
                &format!("b{bi}"),
                &target.branch,
                include_rollup,
            ));
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

/// Reads one resolved `ref` node into a badge. `None` for a node without a
/// readable open PR; [`resolution_from_ref`] decides whether that means "no PR"
/// (an explicit negative) or "malformed" (unresolved) before delegating here.
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
        // The commit this verdict is about — the remote branch head the rollup was
        // read from, not the PR's own `headRefOid` (the same commit, one fewer field
        // to ask for).
        head_oid: node
            .get("target")
            .and_then(|t| t.get("oid"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Reads one **non-null** `ref` node into a resolution: [`PrResolution::NoPr`]
/// when the PR list is present and empty, a badge for an open PR, and `None` —
/// *unresolved*, never a negative — for a malformed node. A false negative would
/// silence the client fallback for a branch that may well have a PR, so anything
/// we cannot positively read stays unresolved (#1370).
fn resolution_from_ref(node: &Value) -> Option<PrResolution> {
    let prs = node
        .get("associatedPullRequests")?
        .get("nodes")?
        .as_array()?;
    if prs.is_empty() {
        return Some(PrResolution::NoPr);
    }
    badge_from_ref(node).map(PrResolution::Pr)
}

/// Reads the GraphQL reply back into resolutions, keyed by target.
///
/// Tri-state per target (#1370): an open PR yields its badge, a checked branch
/// with none — including one never pushed, whose aliased `ref` comes back null —
/// yields [`PrResolution::NoPr`], and a target whose part of the reply is missing
/// or malformed is simply **absent** (unresolved). Best-effort per branch: one
/// bad node never sinks the whole poll, and never mints a negative.
fn parse_response(body: &Value, index: &QueryIndex) -> HashMap<PrTarget, PrResolution> {
    let mut out = HashMap::new();
    let Some(data) = body.get("data") else {
        return out;
    };
    for ((ri, bi), target) in index {
        // A repo alias absent or null (a repo-level failure) is unresolved — only
        // an answer *about the ref* may mint a negative.
        let Some(repo) = data.get(format!("r{ri}")).filter(|r| !r.is_null()) else {
            continue;
        };
        let Some(node) = repo.get(format!("b{bi}")) else {
            continue;
        };
        // A null ref is a branch that does not exist on the remote (never
        // pushed): checked, and definitively PR-less.
        if node.is_null() {
            out.insert(target.clone(), PrResolution::NoPr);
            continue;
        }
        if let Some(resolution) = resolution_from_ref(node) {
            out.insert(target.clone(), resolution);
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
    let query_arg = format!("query={query}");
    let output = crate::github_metrics::run_gh(
        bin,
        ["api", "graphql", "-f", query_arg.as_str()],
        "api graphql",
        None,
    )
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

/// Resolves every target in **one** `gh api graphql` call, using the `gh` at
/// `bin`.
///
/// The binary is a parameter rather than resolved here so callers read the
/// environment **once** (the poller does it at spawn, like its interval) and so
/// tests inject a stub without mutating the process environment — two parallel
/// tests pointing one global env var at different fakes race, and the project is
/// migrating away from test env locks toward injection (#1030).
///
/// **Blocking** — run on a blocking thread. Every target that was successfully
/// checked appears in the map: [`PrResolution::Pr`] for an open PR,
/// [`PrResolution::NoPr`] for a branch — pushed or not — with none (#1370). A
/// target is *absent* only when its part of the reply was missing or malformed,
/// and a failed call (`Err`, including a GraphQL 200 carrying `errors`) yields no
/// map at all — so a failed poll can never manufacture negatives.
pub fn resolve_with(bin: &Path, targets: &[PrTarget]) -> Result<HashMap<PrTarget, PrResolution>> {
    run_query(bin, build_query(targets))
}

/// Like [`resolve_with`] but with the **metadata-only** query (#1384).
///
/// No `statusCheckRollup`, so it costs one node per ref instead of up to 100. The
/// webhook source uses it for webhook-backed repos, whose CI verdict already
/// arrives via events; the returned badges carry PR metadata and `head_oid` with
/// `checks: None`, which the webhook overlay then supersedes for active branches.
pub fn resolve_metadata_with(
    bin: &Path,
    targets: &[PrTarget],
) -> Result<HashMap<PrTarget, PrResolution>> {
    run_query(bin, build_metadata_query(targets))
}

/// Runs a built query (or returns an empty map for `None`), surfacing a GraphQL
/// `errors` array as a hard failure so a 200-with-errors never looks like "no PRs".
fn run_query(
    bin: &Path,
    built: Option<(String, QueryIndex)>,
) -> Result<HashMap<PrTarget, PrResolution>> {
    let Some((query, index)) = built else {
        return Ok(HashMap::new());
    };
    let body = run_gh_graphql(bin, &query)?;
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            bail!("gh api graphql returned errors: {errors:?}");
        }
    }
    Ok(parse_response(&body, &index))
}

/// The poller-written, snapshot-read resolution cache.
///
/// A plain `std::Mutex` map: writes come from the poll loop, reads from the tree
/// snapshot build. The lock is never held across an `.await` — every method takes
/// it, finishes, and drops it.
#[derive(Debug, Default)]
pub struct PrStatusCache {
    resolutions: Mutex<HashMap<PrTarget, PrResolution>>,
}

impl PrStatusCache {
    /// An empty cache. Until the first poll lands, every lookup misses and the
    /// tree simply renders no badge — and no negative either (#1370).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The resolution for one (repo, branch): a badge, an explicit no-PR, or
    /// `None` when never successfully checked.
    #[must_use]
    pub fn get(&self, owner: &str, name: &str, branch: &str) -> Option<PrResolution> {
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
    /// poll — the cost this whole design exists to avoid. A first round of
    /// negatives *is* a change — the one push that delivers them — after which
    /// identical polls stay silent.
    pub fn replace(&self, next: HashMap<PrTarget, PrResolution>) -> bool {
        let mut guard = self.lock();
        if *guard == next {
            return false;
        }
        *guard = next;
        true
    }

    /// Whether any cached badge is still pending — the poller's cadence signal. A
    /// negative is terminal and never holds the fast cadence.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.lock()
            .values()
            .any(|r| matches!(r, PrResolution::Pr(b) if b.checks == PrCheckState::Pending))
    }

    /// Poison-tolerant lock: a panicking holder must not wedge the badge cache,
    /// which is best-effort decoration.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PrTarget, PrResolution>> {
        self.resolutions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};
    use serde_json::json;
    use std::sync::MutexGuard;

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

    // --- Suite awareness: the `needs:`-gate false green ---

    fn run(conclusion: &str, suite: Option<&str>) -> Value {
        let mut e = json!({
            "__typename": "CheckRun",
            "status": "COMPLETED",
            "conclusion": conclusion,
        });
        if let Some(s) = suite {
            e["checkSuite"] = json!({ "status": s });
        }
        e
    }

    #[test]
    fn rollup_is_pending_while_a_suite_is_still_creating_jobs() {
        // The exact shape observed on PRs #1294/#1329: every check run that exists
        // is green — GitHub's own rollup `state` reads SUCCESS — but the CI suite is
        // still spawning the `needs: gate` fan-out. Reporting success here is the
        // false ✓ this issue is about.
        let contexts = vec![
            run("SUCCESS", Some("IN_PROGRESS")),
            run("SUCCESS", Some("IN_PROGRESS")),
        ];
        assert_eq!(rollup_check_state(&contexts), PrCheckState::Pending);
        // QUEUED counts too — the suite exists but has not started its fan-out.
        assert_eq!(
            rollup_check_state(&[run("SUCCESS", Some("QUEUED"))]),
            PrCheckState::Pending
        );
    }

    #[test]
    fn rollup_is_success_once_every_backing_suite_is_terminal() {
        let contexts = vec![
            run("SUCCESS", Some("COMPLETED")),
            run("SKIPPED", Some("COMPLETED")),
        ];
        assert_eq!(rollup_check_state(&contexts), PrCheckState::Success);
    }

    #[test]
    fn rollup_ignores_a_zero_run_zombie_suite() {
        // codecov leaves a QUEUED suite with **zero** check runs on every PR — one
        // was still queued after 3.7 days. A rule keyed on "any non-terminal suite"
        // would pin such a PR yellow forever. Keying on suites reachable *through a
        // check run* excludes it structurally: with no runs, it never appears in the
        // rollup at all. So a rollup whose runs all carry terminal suites is green,
        // even though a zombie suite exists on the commit.
        let contexts = vec![run("SUCCESS", Some("COMPLETED"))];
        assert_eq!(rollup_check_state(&contexts), PrCheckState::Success);
    }

    #[test]
    fn rollup_tolerates_entries_without_suite_information() {
        // A legacy StatusContext has no `checkSuite`, and neither does a CheckRun if
        // the field is ever absent. Missing suite info must not imply "still
        // running", or every StatusContext-only repo pins yellow.
        assert_eq!(
            rollup_check_state(&[json!({"__typename":"StatusContext","state":"SUCCESS"})]),
            PrCheckState::Success
        );
        assert_eq!(
            rollup_check_state(&[run("SUCCESS", None)]),
            PrCheckState::Success
        );
        // An empty suite status is not a running suite either.
        assert_eq!(
            rollup_check_state(&[run("SUCCESS", Some(""))]),
            PrCheckState::Success
        );
    }

    #[test]
    fn rollup_failure_still_dominates_a_running_suite() {
        // A red check is red regardless of what else is still spawning.
        let contexts = vec![
            run("FAILURE", Some("IN_PROGRESS")),
            run("SUCCESS", Some("IN_PROGRESS")),
        ];
        assert_eq!(rollup_check_state(&contexts), PrCheckState::Failure);
    }

    #[test]
    fn build_query_asks_for_the_backing_suite_status() {
        let (query, _) = build_query(&[target("main")]).unwrap();
        assert!(query.contains("checkSuite{ status }"), "{query}");
    }

    #[test]
    fn build_metadata_query_omits_the_costly_rollup() {
        // #1384: the metadata-only reconcile drops `statusCheckRollup` (the
        // per-ref, up-to-100-node cost driver) but keeps `oid` (for staleness) and
        // the open-PR metadata. The full query still carries the rollup.
        let (full, _) = build_query(&[target("main")]).unwrap();
        assert!(full.contains("statusCheckRollup"), "{full}");

        let (meta, _) = build_metadata_query(&[target("main")]).unwrap();
        assert!(!meta.contains("statusCheckRollup"), "{meta}");
        assert!(meta.contains("...on Commit{ oid"), "{meta}");
        assert!(meta.contains("associatedPullRequests(first:1"), "{meta}");
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
    fn parse_response_reads_badges_and_resolves_absent_refs_as_negatives() {
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
            // An unpushed branch resolves to a null ref — checked, and PR-less.
            "b1": null,
            // Pushed, but no open PR heads it.
            "b2": {
                "target": {"oid":"def","statusCheckRollup":null},
                "associatedPullRequests":{"nodes":[]}
            }
        }}});
        let out = parse_response(&body, &index);
        assert_eq!(out.len(), 3, "{out:?}");
        let Some(PrResolution::Pr(badge)) = out.get(&target("feature")) else {
            panic!("expected a badge for feature: {out:?}");
        };
        assert_eq!(badge.number, 65);
        assert!(badge.is_draft);
        assert_eq!(badge.checks, PrCheckState::Success);
        assert_eq!(badge.url, "u");
        assert_eq!(out.get(&target("unpushed")), Some(&PrResolution::NoPr));
        assert_eq!(out.get(&target("no-pr")), Some(&PrResolution::NoPr));
    }

    #[test]
    fn parse_response_reports_a_pushed_branch_with_no_pr_as_a_negative() {
        let (_, index) = build_query(&[target("quiet")]).unwrap();
        let body = json!({"data":{"r0":{"b0":{
            "target": {"oid":"abc","statusCheckRollup":null},
            "associatedPullRequests":{"nodes":[]}
        }}}});
        let out = parse_response(&body, &index);
        assert_eq!(out.get(&target("quiet")), Some(&PrResolution::NoPr));
    }

    #[test]
    fn parse_response_reports_an_unpushed_branch_as_a_negative() {
        let (_, index) = build_query(&[target("local-only")]).unwrap();
        let body = json!({"data":{"r0":{"b0":null}}});
        let out = parse_response(&body, &index);
        assert_eq!(out.get(&target("local-only")), Some(&PrResolution::NoPr));
    }

    #[test]
    fn parse_response_leaves_a_missing_alias_unresolved_rather_than_negative() {
        // Only an answer about the ref may mint a negative: a reply missing the
        // ref alias, or with a null repo alias (a repo-level failure), says
        // nothing about the branch — a false NoPr would silence the client
        // fallback for a branch that may have a PR.
        let (_, index) = build_query(&[target("x")]).unwrap();
        assert!(parse_response(&json!({"data":{"r0":{}}}), &index).is_empty());
        assert!(parse_response(&json!({"data":{"r0":null}}), &index).is_empty());
    }

    #[test]
    fn parse_response_leaves_a_malformed_pr_node_unresolved_rather_than_negative() {
        let (_, index) = build_query(&[target("x")]).unwrap();
        // The PR list is non-empty, so this is not "no PR" — but the node has no
        // readable number, so it is not a badge either. It must stay unresolved.
        let body = json!({"data":{"r0":{"b0":{
            "target": {"oid":"abc","statusCheckRollup":null},
            "associatedPullRequests":{"nodes":[{"isDraft":false,"url":"u"}]}
        }}}});
        assert!(parse_response(&body, &index).is_empty());
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
        // A PR with no checks is still a PR — PrCheckState::None, never NoPr.
        let Some(PrResolution::Pr(badge)) = out.get(&target("feature")) else {
            panic!("expected a badge: {out:?}");
        };
        assert_eq!(badge.checks, PrCheckState::None);
    }

    #[test]
    fn parse_response_tolerates_a_missing_data_block() {
        // A reply without `data` resolves nothing — and mints no negatives.
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
            head_oid: String::new(),
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
    ///
    /// Returns the shim serialisation lock alongside the path: the caller
    /// **must** hold the guard until it has finished exec'ing the stub, so
    /// concurrent shim subprocesses stay bounded. That lock does **not** close
    /// the `ETXTBSY` ("Text file busy") race — writing an executable and then
    /// `execve`ing it races every other thread that forks, which never takes
    /// this lock — so the caller also runs the exec through [`retry_on_etxtbsy`]
    /// (#642, #1344, #1348).
    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n"),
        );
        (path, guard)
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
        let (bin, _shim) = fake_gh(dir.path(), "", 1);
        let err = retry_on_etxtbsy(|| resolve_with(&bin, &[target("main")])).unwrap_err();
        assert!(
            format!("{err:#}").contains("gh api graphql failed"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_with_errors_on_unparseable_output() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path(), "not json at all", 0);
        let err = retry_on_etxtbsy(|| resolve_with(&bin, &[target("main")])).unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"), "{err:#}");
    }

    #[test]
    fn resolve_with_surfaces_graphql_errors_rather_than_reporting_no_badges() {
        // A GraphQL 200 can still carry errors (a bad field, a rate limit). Reading
        // that as an empty result would be indistinguishable from "no open PRs" and
        // would silently blank every badge, so it must be an error. Doubly
        // load-bearing since #1370: the hard Err is what guarantees a failed poll
        // can never manufacture NoPr negatives.
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            r#"{"data":null,"errors":[{"message":"API rate limit exceeded"}]}"#,
            0,
        );
        let err = retry_on_etxtbsy(|| resolve_with(&bin, &[target("main")])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("returned errors"), "{msg}");
        assert!(msg.contains("rate limit"), "{msg}");
    }

    #[test]
    fn resolve_with_ignores_an_empty_errors_array() {
        // Some responses carry `errors: []`; that is a success, not a failure.
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            r#"{"errors":[],"data":{"r0":{"b0":{
                "target":{"oid":"a","statusCheckRollup":null},
                "associatedPullRequests":{"nodes":[{"number":9,"isDraft":false,"url":"u"}]}}}}}"#,
            0,
        );
        let out = retry_on_etxtbsy(|| resolve_with(&bin, &[target("main")])).unwrap();
        let Some(PrResolution::Pr(badge)) = out.get(&target("main")) else {
            panic!("expected a badge: {out:?}");
        };
        assert_eq!(badge.number, 9);
    }

    #[test]
    fn resolve_with_reads_a_real_reply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            r#"{"data":{"r0":{"b0":{
                "target":{"oid":"a","statusCheckRollup":{"contexts":{"nodes":[
                  {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":42,"isDraft":true,"url":"u42"}]}}}}}"#,
            0,
        );
        let out = retry_on_etxtbsy(|| resolve_with(&bin, &[target("main")])).unwrap();
        let Some(PrResolution::Pr(badge)) = out.get(&target("main")) else {
            panic!("expected a badge: {out:?}");
        };
        assert_eq!(badge.number, 42);
        assert!(badge.is_draft);
        assert_eq!(badge.checks, PrCheckState::Failure);
    }

    #[test]
    fn resolve_with_reports_negatives_alongside_badges_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(
            dir.path(),
            r#"{"data":{"r0":{
                "b0":{
                    "target":{"oid":"a","statusCheckRollup":null},
                    "associatedPullRequests":{"nodes":[{"number":7,"isDraft":false,"url":"u"}]}},
                "b1":null,
                "b2":{
                    "target":{"oid":"b","statusCheckRollup":null},
                    "associatedPullRequests":{"nodes":[]}}}}}"#,
            0,
        );
        let targets = vec![target("main"), target("unpushed"), target("quiet")];
        let out = retry_on_etxtbsy(|| resolve_with(&bin, &targets)).unwrap();
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(
            matches!(out.get(&target("main")), Some(PrResolution::Pr(b)) if b.number == 7),
            "{out:?}"
        );
        assert_eq!(out.get(&target("unpushed")), Some(&PrResolution::NoPr));
        assert_eq!(out.get(&target("quiet")), Some(&PrResolution::NoPr));
    }

    // --- Cache ---

    /// A pending badge wrapped as a resolution, for cache fixtures.
    fn pending_pr(number: u64) -> PrResolution {
        PrResolution::Pr(PrBadge {
            number,
            is_draft: false,
            checks: PrCheckState::Pending,
            url: "u".into(),
            head_oid: String::new(),
        })
    }

    #[test]
    fn cache_replace_reports_whether_anything_changed() {
        let cache = PrStatusCache::new();
        let mut map = HashMap::new();
        map.insert(target("f"), pending_pr(1));

        // First write is a change.
        assert!(cache.replace(map.clone()));
        // An identical write is not — this is what keeps the poller from re-pushing
        // an unchanged snapshot to every window on every tick.
        assert!(!cache.replace(map.clone()));

        // A changed verdict is a change.
        let mut moved = map.clone();
        if let Some(PrResolution::Pr(badge)) = moved.get_mut(&target("f")) {
            badge.checks = PrCheckState::Success;
        }
        assert!(cache.replace(moved));
        // Emptying is a change.
        assert!(cache.replace(HashMap::new()));
        assert!(!cache.replace(HashMap::new()));
    }

    #[test]
    fn cache_replace_counts_a_new_negative_as_a_change() {
        let cache = PrStatusCache::new();
        let mut negatives = HashMap::new();
        negatives.insert(target("f"), PrResolution::NoPr);

        // The first round of negatives is a change — the one push that delivers
        // them to clients so their fallback goes quiet.
        assert!(cache.replace(negatives.clone()));
        // Re-polling the same answer is not.
        assert!(!cache.replace(negatives.clone()));

        // A PR opening (negative → badge) and closing (badge → negative) are both
        // real transitions.
        let mut opened = HashMap::new();
        opened.insert(target("f"), pending_pr(1));
        assert!(cache.replace(opened));
        assert!(cache.replace(negatives));
    }

    #[test]
    fn cache_any_pending_ignores_negative_resolutions() {
        // A negative is terminal: it must never hold the poller's fast cadence.
        let cache = PrStatusCache::new();
        let mut map = HashMap::new();
        map.insert(target("f"), PrResolution::NoPr);
        cache.replace(map);
        assert!(!cache.any_pending());
    }

    #[test]
    fn cache_get_and_any_pending() {
        let cache = PrStatusCache::new();
        assert!(cache.get("rust-works", "omni-dev", "f").is_none());
        assert!(!cache.any_pending());

        let mut map = HashMap::new();
        map.insert(target("f"), pending_pr(1));
        cache.replace(map.clone());
        assert!(
            matches!(
                cache.get("rust-works", "omni-dev", "f"),
                Some(PrResolution::Pr(b)) if b.number == 1
            ),
            "expected the cached badge back"
        );
        assert!(cache.any_pending());
        // A miss on a branch we never resolved.
        assert!(cache.get("rust-works", "omni-dev", "other").is_none());

        if let Some(PrResolution::Pr(badge)) = map.get_mut(&target("f")) {
            badge.checks = PrCheckState::Success;
        }
        cache.replace(map);
        assert!(!cache.any_pending());
    }
}
