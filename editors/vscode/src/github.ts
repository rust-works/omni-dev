// Pull-request discovery, formatting, and URI building for the "Open Pull
// Request…" action on the Worktrees view.
//
// Like `tree.ts`/`socket.ts`, this module is deliberately free of any `vscode`
// import so it stays pure and unit-testable under `node --test`. The real `gh`
// subprocess runner lives in `gh.ts`; the `vscode`-facing command glue that
// drives the quick-picks lives in `prCommands.ts`. Everything with logic worth
// asserting — the `gh` arg arrays, the defensive JSON parse, the quick-pick
// labels, the URI, and the scope→discovery mapping — is here behind an injected
// runner so it can be exercised without a real `gh` or a real editor.

import { PrBadge, PrCheckState, TreeGithubIdentity } from "./tree";

/**
 * One entry of a PR's `statusCheckRollup` (#1296), as `gh pr list --json
 * statusCheckRollup` emits it. The array mixes GitHub's two check types — a
 * `CheckRun` (Actions/apps: `status` plus a `conclusion` once complete) and a
 * `StatusContext` (the legacy commit-status API: a single `state`) — so every
 * field is optional and {@link rollupCheckState} reduces across whichever are
 * present.
 */
export interface StatusCheckRollupEntry {
  /** `"CheckRun"` or `"StatusContext"`; informational, not required to classify. */
  __typename?: string;
  /** CheckRun lifecycle: `QUEUED` | `IN_PROGRESS` | `COMPLETED` | …. */
  status?: string;
  /** CheckRun verdict once `COMPLETED`: `SUCCESS` | `FAILURE` | `NEUTRAL` | …. */
  conclusion?: string;
  /** StatusContext verdict: `SUCCESS` | `FAILURE` | `ERROR` | `PENDING` | `EXPECTED`. */
  state?: string;
}

/**
 * One open pull request, as returned by `gh pr list --json …`. Only the fields
 * this feature requests are modelled; `gh` guarantees their presence for the
 * `--json` keys we pass, so they are non-optional except `author` (which `gh`
 * can report as an object with a missing `login` for some actors) and
 * `statusCheckRollup` (requested only by the badge path, {@link prBadgeListArgs}).
 */
export interface PullRequest {
  number: number;
  title: string;
  url: string;
  headRefName: string;
  baseRefName: string;
  isDraft: boolean;
  state: string;
  author?: { login?: string; name?: string };
  statusCheckRollup?: StatusCheckRollupEntry[];
}

/** The `--json` fields requested from `gh pr list` — mirrors {@link PullRequest}. */
export const PR_JSON_FIELDS = "number,title,url,headRefName,baseRefName,isDraft,state,author";

/**
 * The `--json` fields for the tree PR **badge** (#1296): the base fields plus the
 * verbose `statusCheckRollup` needed for the checks glyph. Kept separate from
 * {@link PR_JSON_FIELDS} so the "Open Pull Request…" quick-pick — which never
 * shows check state — does not pay to fetch the rollup.
 */
export const PR_BADGE_JSON_FIELDS = `${PR_JSON_FIELDS},statusCheckRollup`;

/** The `gh pr list --limit` cap — high enough to list a repo's open PRs in one call. */
export const PR_LIST_LIMIT = "100";

/**
 * A `gh` invocation: given an argv (no leading `gh`), resolve with its stdout,
 * or reject with an actionable error. The real implementation is in `gh.ts`;
 * tests inject a fake so discovery can be exercised without a subprocess.
 */
export type GhRunner = (args: string[]) => Promise<string>;

/**
 * What to look for. A **repo** scope lists every open PR of the repository; a
 * **worktree** scope lists only the PR(s) whose head branch is the worktree's
 * checked-out branch (and yields nothing for a detached/unborn worktree, whose
 * `branch` is absent).
 */
export type PrScope =
  | { kind: "repo"; repo: TreeGithubIdentity }
  | { kind: "worktree"; repo: TreeGithubIdentity; branch?: string };

/** The `owner/name` slug `gh --repo` expects. */
function repoSlug(repo: TreeGithubIdentity): string {
  return `${repo.owner}/${repo.name}`;
}

/** Builds the `gh pr list` argv for **all** of a repo's open PRs. */
export function prListArgsForRepo(repo: TreeGithubIdentity): string[] {
  return [
    "pr",
    "list",
    "--repo",
    repoSlug(repo),
    "--state",
    "open",
    "--json",
    PR_JSON_FIELDS,
    "--limit",
    PR_LIST_LIMIT,
  ];
}

/** Builds the `gh pr list` argv for the open PR(s) whose head is `branch`. */
export function prListArgsForBranch(repo: TreeGithubIdentity, branch: string): string[] {
  return [
    "pr",
    "list",
    "--repo",
    repoSlug(repo),
    "--head",
    branch,
    "--state",
    "open",
    "--json",
    PR_JSON_FIELDS,
    "--limit",
    PR_LIST_LIMIT,
  ];
}

/**
 * Builds the `gh pr list` argv for **all** of a repo's open PRs *with* their
 * `statusCheckRollup` (#1296) — the one call the tree makes per repo-expand to
 * badge every worktree. Like {@link prListArgsForRepo} but requesting the extra
 * checks field.
 */
export function prBadgeListArgs(repo: TreeGithubIdentity): string[] {
  return [
    "pr",
    "list",
    "--repo",
    repoSlug(repo),
    "--state",
    "open",
    "--json",
    PR_BADGE_JSON_FIELDS,
    "--limit",
    PR_LIST_LIMIT,
  ];
}

/**
 * Parses `gh pr list --json` stdout into a `PullRequest[]`, defensively: empty
 * stdout (never emitted by `gh`, but cheap to tolerate) is an empty list, and
 * anything that is not a JSON array is an actionable error rather than a thrown
 * `SyntaxError` bubbling out of the extension host.
 */
export function parsePrList(stdout: string): PullRequest[] {
  const trimmed = stdout.trim();
  if (trimmed === "") {
    return [];
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    throw new Error("could not parse `gh pr list` output as JSON");
  }
  if (!Array.isArray(parsed)) {
    throw new Error("unexpected `gh pr list` output: expected a JSON array");
  }
  return parsed as PullRequest[];
}

/**
 * Discovers the open PR(s) in scope, running `gh` through the injected runner.
 * A worktree with no branch (detached/unborn) can have no head-matching PR, so
 * it resolves to `[]` **without** invoking the runner at all.
 */
export async function discoverPullRequests(
  scope: PrScope,
  runner: GhRunner,
): Promise<PullRequest[]> {
  let args: string[];
  if (scope.kind === "repo") {
    args = prListArgsForRepo(scope.repo);
  } else {
    if (!scope.branch) {
      return [];
    }
    args = prListArgsForBranch(scope.repo, scope.branch);
  }
  return parsePrList(await runner(args));
}

/** The quick-pick label for a PR: `#<number> <title>`. */
export function prQuickPickLabel(pr: PullRequest): string {
  return `#${pr.number} ${pr.title}`;
}

/**
 * The muted quick-pick description for a PR: `<head> → <base>`, then `draft`
 * and the author handle when present, joined by a middot.
 */
export function prQuickPickDescription(pr: PullRequest): string {
  const parts = [`${pr.headRefName} → ${pr.baseRefName}`];
  if (pr.isDraft) {
    parts.push("draft");
  }
  if (pr.author?.login) {
    parts.push(`@${pr.author.login}`);
  }
  return parts.join(" · ");
}

/**
 * Builds the URI that asks the **GitHub Pull Requests** extension
 * (`GitHub.vscode-pull-request-github`) to open a PR's overview as a tab:
 *
 * ```
 * <scheme>://github.vscode-pull-request-github/open-pull-request-webview?uri=<pr web url>
 * ```
 *
 * The handler (verified against v0.156.0) takes a **single** `uri` query
 * parameter holding the PR's full `github.com` web URL and matches it against
 * `^https?://github.com/<owner>/<repo>/pull/<n>$` — so we pass the PR's `url`
 * verbatim (exactly that shape, straight from `gh pr list --json url`) rather
 * than separate owner/repo/number params, which this handler does not accept.
 *
 * `scheme` is the running product's URI scheme (`vscode.env.uriScheme` —
 * `vscode`, `vscode-insiders`, `cursor`, …) so the handler is reached in every
 * VS Code-family editor. Siblings not used here: `open-pull-request-changes`
 * (diff tab) and `checkout-pull-request`.
 */
export function prOverviewUri(scheme: string, prWebUrl: string): string {
  const query = new URLSearchParams({ uri: prWebUrl }).toString();
  return `${scheme}://github.vscode-pull-request-github/open-pull-request-webview?${query}`;
}

// GitHub's check conclusions / status-context states, bucketed. Names are
// upper-cased before lookup, so the sets hold the canonical GraphQL enum values.
const FAILURE_STATES = new Set([
  "FAILURE",
  "ERROR",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
  "STALE",
]);
// A skipped/neutral check is non-blocking, so it counts toward `success` (matching
// how `gh pr checks` treats "skipping" as a pass, not a failure).
const SUCCESS_STATES = new Set(["SUCCESS", "NEUTRAL", "SKIPPED"]);

/**
 * Classifies one rollup entry. A CheckRun that has not `COMPLETED` is pending
 * regardless of its (null) conclusion; otherwise the conclusion (CheckRun) or
 * state (StatusContext) decides. Anything unrecognized is treated as pending so a
 * still-resolving or unknown check never reads as passing.
 */
function checkEntryState(entry: StatusCheckRollupEntry): PrCheckState {
  if (entry.status !== undefined && entry.status !== "" && entry.status.toUpperCase() !== "COMPLETED") {
    return "pending";
  }
  const raw = (entry.conclusion || entry.state || "").toUpperCase();
  if (FAILURE_STATES.has(raw)) {
    return "failure";
  }
  if (SUCCESS_STATES.has(raw)) {
    return "success";
  }
  // Everything else — a pending state (PENDING/EXPECTED/…), an empty verdict
  // (completed-but-unset), or an unknown value — falls through to pending, so a
  // still-resolving or unrecognized check never reads as a false pass.
  return "pending";
}

/**
 * Reduces a PR's `statusCheckRollup` to one verdict (#1296): any failing check
 * dominates (`failure`); else any still-running one (`pending`); else, if at least
 * one succeeded, `success`; an empty or absent rollup means no checks (`none`).
 */
export function rollupCheckState(rollup?: StatusCheckRollupEntry[]): PrCheckState {
  if (!rollup || rollup.length === 0) {
    return "none";
  }
  let sawPending = false;
  let sawSuccess = false;
  for (const entry of rollup) {
    switch (checkEntryState(entry)) {
      case "failure":
        return "failure";
      case "pending":
        sawPending = true;
        break;
      case "success":
        sawSuccess = true;
        break;
      case "none":
        break;
    }
  }
  if (sawPending) {
    return "pending";
  }
  return sawSuccess ? "success" : "none";
}

/**
 * Finds the open PR whose head is `branch` and reduces it to a {@link PrBadge}
 * for the tree (#1296): the first head-branch match (mirroring the issue's "first
 * match" rule), with its checks rolled up. Returns `undefined` for a
 * detached/unborn worktree (no `branch`) or when no open PR heads that branch.
 */
export function prBadgeForBranch(prs: PullRequest[], branch?: string): PrBadge | undefined {
  if (!branch) {
    return undefined;
  }
  const pr = prs.find((p) => p.headRefName === branch);
  if (!pr) {
    return undefined;
  }
  return {
    number: pr.number,
    isDraft: pr.isDraft,
    checks: rollupCheckState(pr.statusCheckRollup),
    url: pr.url,
  };
}
