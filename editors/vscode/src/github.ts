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

import { TreeGithubIdentity } from "./tree";

/**
 * One open pull request, as returned by `gh pr list --json …`. Only the fields
 * this feature requests are modelled; `gh` guarantees their presence for the
 * `--json` keys we pass, so they are non-optional except `author` (which `gh`
 * can report as an object with a missing `login` for some actors).
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
}

/** The `--json` fields requested from `gh pr list` — mirrors {@link PullRequest}. */
export const PR_JSON_FIELDS = "number,title,url,headRefName,baseRefName,isDraft,state,author";

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
 * <scheme>://github.vscode-pull-request-github/open-pull-request-webview?owner=…&repo=…&pullRequestNumber=…
 * ```
 *
 * `scheme` is the running product's URI scheme (`vscode.env.uriScheme` —
 * `vscode`, `vscode-insiders`, `cursor`, …) so the handler is reached in every
 * VS Code-family editor. Siblings not used here: `open-pull-request-changes`
 * (diff tab) and `checkout-pull-request`.
 */
export function prOverviewUri(
  scheme: string,
  owner: string,
  name: string,
  number: number,
): string {
  const query = new URLSearchParams({
    owner,
    repo: name,
    pullRequestNumber: String(number),
  }).toString();
  return `${scheme}://github.vscode-pull-request-github/open-pull-request-webview?${query}`;
}
