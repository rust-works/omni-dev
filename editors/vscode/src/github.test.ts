// Unit tests for the pure PR-discovery module. Nothing here imports `vscode`,
// so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  PR_JSON_FIELDS,
  PR_LIST_LIMIT,
  PullRequest,
  dedupePullRequests,
  discoverPullRequests,
  parsePrList,
  prFallbackBadge,
  prListArgsForRepo,
  prOverviewUri,
  prQuickPickDescription,
  prQuickPickLabel,
  prScopeForNode,
  prScopesForNodes,
  pullRequestFromBadge,
  scopeLabel,
} from "./github";
import { Node, PrBadge, TreeGithubIdentity, TreeRepoPayload } from "./tree";

const REPO: TreeGithubIdentity = { owner: "rust-works", name: "omni-dev" };

const PR: PullRequest = {
  number: 1299,
  title: "Open a PR from the Worktrees view",
  url: "https://github.com/rust-works/omni-dev/pull/1299",
  headRefName: "issue-1299-open-pr-from-worktrees-view",
  baseRefName: "main",
  isDraft: false,
  state: "OPEN",
  author: { login: "newhoggy", name: "John Ky" },
};

test("prListArgsForRepo lists all open PRs of the repo", () => {
  assert.deepEqual(prListArgsForRepo(REPO), [
    "pr",
    "list",
    "--repo",
    "rust-works/omni-dev",
    "--state",
    "open",
    "--json",
    PR_JSON_FIELDS,
    "--limit",
    PR_LIST_LIMIT,
  ]);
});

test("parsePrList tolerates empty stdout and an empty array", () => {
  assert.deepEqual(parsePrList(""), []);
  assert.deepEqual(parsePrList("   \n"), []);
  assert.deepEqual(parsePrList("[]"), []);
});

test("parsePrList returns the parsed PRs for a valid array", () => {
  const prs = parsePrList(JSON.stringify([PR]));
  assert.equal(prs.length, 1);
  assert.equal(prs[0].number, 1299);
  assert.equal(prs[0].headRefName, "issue-1299-open-pr-from-worktrees-view");
});

test("parsePrList throws an actionable error on malformed or non-array output", () => {
  assert.throws(() => parsePrList("not json at all"), /could not parse/);
  assert.throws(() => parsePrList('{"number":1}'), /expected a JSON array/);
});

test("prOverviewUri builds the webview URI with the PR url as the `uri` param", () => {
  // The handler wants a single `uri` param holding the full github.com PR URL,
  // URL-encoded; owner/repo/number are parsed out of it by the extension.
  assert.equal(
    prOverviewUri("vscode", "https://github.com/rust-works/omni-dev/pull/1299"),
    "vscode://github.vscode-pull-request-github/open-pull-request-webview?uri=https%3A%2F%2Fgithub.com%2Frust-works%2Fomni-dev%2Fpull%2F1299",
  );
  // The scheme is the running product's, so the handler is reached in forks too.
  assert.match(
    prOverviewUri("cursor", "https://github.com/o/r/pull/42"),
    /^cursor:\/\/github\.vscode-pull-request-github\/open-pull-request-webview\?uri=/,
  );
});

test("prQuickPickLabel is `#<number> <title>`", () => {
  assert.equal(prQuickPickLabel(PR), "#1299 Open a PR from the Worktrees view");
});

test("prQuickPickDescription shows head→base, then draft and author when present", () => {
  assert.equal(
    prQuickPickDescription(PR),
    "issue-1299-open-pr-from-worktrees-view → main · @newhoggy",
  );
  assert.equal(
    prQuickPickDescription({ ...PR, isDraft: true }),
    "issue-1299-open-pr-from-worktrees-view → main · draft · @newhoggy",
  );
  // No author login → just the branch pair.
  assert.equal(prQuickPickDescription({ ...PR, author: undefined }), "issue-1299-open-pr-from-worktrees-view → main");
});

/** A `RepoPrFetcher` that records the repos it was asked about and returns `prs`. */
function recordingFetcher(prs: PullRequest[]): {
  fetch: (repo: TreeGithubIdentity) => Promise<PullRequest[]>;
  calls: TreeGithubIdentity[];
} {
  const calls: TreeGithubIdentity[] = [];
  return {
    calls,
    fetch: async (repo) => {
      calls.push(repo);
      return prs;
    },
  };
}

const BADGE: PrBadge = {
  number: 1299,
  isDraft: false,
  checks: "success",
  url: "https://github.com/rust-works/omni-dev/pull/1299",
};

test("discoverPullRequests lists all PRs for a repo scope via the fetcher", async () => {
  const { fetch, calls } = recordingFetcher([PR]);
  const prs = await discoverPullRequests({ kind: "repo", repo: REPO }, fetch);
  assert.deepEqual(prs, [PR]);
  assert.deepEqual(calls, [REPO]);
});

test("discoverPullRequests answers a badged worktree from the snapshot with no fetch (#1389)", async () => {
  const { fetch, calls } = recordingFetcher([PR]);
  const prs = await discoverPullRequests(
    { kind: "worktree", repo: REPO, branch: "issue-1299", badge: BADGE },
    fetch,
  );
  assert.deepEqual(prs, [pullRequestFromBadge(BADGE, "issue-1299")]);
  assert.equal(calls.length, 0, "a resolved badge must not spend a fetch");
});

test("discoverPullRequests returns [] for an explicit pr_none without fetching (#1389)", async () => {
  const { fetch, calls } = recordingFetcher([PR]);
  const prs = await discoverPullRequests(
    { kind: "worktree", repo: REPO, branch: "issue-1300", prNone: true },
    fetch,
  );
  assert.deepEqual(prs, []);
  assert.equal(calls.length, 0);
});

test("discoverPullRequests filters an unresolved worktree's repo list to its branch", async () => {
  // Not-polled repo / old daemon: no badge, no pr_none — fall through to the shared
  // repo list and keep only the head-matching PR.
  const other: PullRequest = { ...PR, number: 7, headRefName: "other", url: "u7" };
  const { fetch, calls } = recordingFetcher([PR, other]);
  const prs = await discoverPullRequests(
    { kind: "worktree", repo: REPO, branch: PR.headRefName },
    fetch,
  );
  assert.deepEqual(prs, [PR]);
  assert.deepEqual(calls, [REPO]);
});

test("discoverPullRequests returns [] for a detached worktree without fetching", async () => {
  const { fetch, calls } = recordingFetcher([PR]);
  const prs = await discoverPullRequests({ kind: "worktree", repo: REPO }, fetch);
  assert.deepEqual(prs, []);
  assert.equal(calls.length, 0);
});

test("pullRequestFromBadge synthesises an openable PR from a snapshot badge", () => {
  assert.deepEqual(pullRequestFromBadge(BADGE, "issue-1299"), {
    number: 1299,
    title: "",
    url: "https://github.com/rust-works/omni-dev/pull/1299",
    headRefName: "issue-1299",
    baseRefName: "",
    isDraft: false,
    state: "OPEN",
  });
});

// --- PR badge (#1296): checks rollup + branch matching -----------------------

// The rollup reducer moved to the daemon with #1337 (badges are resolved there
// now, and kept live by its poller); its coverage moved with it, to the Rust unit
// tests in `src/pr_status.rs`. What remains extension-side is the degraded
// fallback used only against a daemon too old to supply `pr`.

const FALLBACK_PRS: PullRequest[] = [
  {
    number: 65,
    title: "Add thing",
    url: "https://github.com/o/r/pull/65",
    headRefName: "feature",
    baseRefName: "main",
    isDraft: true,
    state: "OPEN",
  },
  {
    number: 66,
    title: "Other",
    url: "https://github.com/o/r/pull/66",
    headRefName: "other",
    baseRefName: "main",
    isDraft: false,
    state: "OPEN",
  },
];

test("prFallbackBadge matches the head branch and carries the PR, never a check state", () => {
  const badge = prFallbackBadge(FALLBACK_PRS, "feature");
  assert.deepEqual(badge, {
    number: 65,
    isDraft: true,
    // Deliberately never a verdict: nothing extension-side polls, so a checks
    // glyph here could not refresh and a stale one is worse than none (#1337).
    checks: "none",
    url: "https://github.com/o/r/pull/65",
  });
});

test("prFallbackBadge returns undefined for no branch or no head match", () => {
  assert.equal(prFallbackBadge(FALLBACK_PRS, undefined), undefined);
  assert.equal(prFallbackBadge(FALLBACK_PRS, "nope"), undefined);
  assert.equal(prFallbackBadge([], "feature"), undefined);
});

test("prFallbackBadge takes the first head match", () => {
  const dupes: PullRequest[] = [
    { ...FALLBACK_PRS[0], number: 1 },
    { ...FALLBACK_PRS[0], number: 2 },
  ];
  assert.equal(prFallbackBadge(dupes, "feature")?.number, 1);
});

// --- Scope mapping and multi-select fan-out (#1357) --------------------------

const SCOPE_REPO: TreeRepoPayload = {
  main_repo: "omni-dev",
  github: REPO,
  root: "/home/me/omni-dev",
  worktrees: [
    { path: "/home/me/omni-dev", branch: "main", is_main: true, open: true },
    { path: "/home/me/wt/a", branch: "a", is_main: false, open: true },
    { path: "/home/me/wt/detached", is_main: false, open: true },
    // A branch the daemon already resolved to an open PR (#1389, fix 7).
    { path: "/home/me/wt/feat", branch: "feat", is_main: false, open: true, pr: BADGE },
  ],
};

/** A repo with no `github` — an origin that is not `github.com`, or none at all. */
const NO_GITHUB_REPO: TreeRepoPayload = {
  main_repo: "scratch",
  root: "/home/me/scratch",
  worktrees: [{ path: "/home/me/scratch", branch: "main", is_main: true, open: true }],
};

const repoNode = (repo: TreeRepoPayload): Node => ({ kind: "repo", repo });
const wtNode = (repo: TreeRepoPayload, i: number): Node => ({
  kind: "worktree",
  repo,
  wt: repo.worktrees[i],
});

test("prScopeForNode maps a repo node to a whole-repo scope", () => {
  assert.deepEqual(prScopeForNode(repoNode(SCOPE_REPO)), { kind: "repo", repo: REPO });
});

test("prScopeForNode maps a worktree node to its branch's scope", () => {
  assert.deepEqual(prScopeForNode(wtNode(SCOPE_REPO, 1)), {
    kind: "worktree",
    repo: REPO,
    branch: "a",
    badge: undefined,
    prNone: undefined,
  });
  // Detached/unborn: a scope with no branch, which discovery resolves to `[]`.
  assert.deepEqual(prScopeForNode(wtNode(SCOPE_REPO, 2)), {
    kind: "worktree",
    repo: REPO,
    branch: undefined,
    badge: undefined,
    prNone: undefined,
  });
});

test("prScopeForNode carries the daemon's resolved badge for a branch (#1389)", () => {
  assert.deepEqual(prScopeForNode(wtNode(SCOPE_REPO, 3)), {
    kind: "worktree",
    repo: REPO,
    branch: "feat",
    badge: BADGE,
    prNone: undefined,
  });
});

test("prScopeForNode yields nothing for a repo with no GitHub identity", () => {
  assert.equal(prScopeForNode(repoNode(NO_GITHUB_REPO)), undefined);
  assert.equal(prScopeForNode(wtNode(NO_GITHUB_REPO, 0)), undefined);
});

test("prScopesForNodes drops nodes with no GitHub identity", () => {
  const scopes = prScopesForNodes([wtNode(NO_GITHUB_REPO, 0), wtNode(SCOPE_REPO, 1)]);
  assert.deepEqual(scopes, [
    { kind: "worktree", repo: REPO, branch: "a", badge: undefined, prNone: undefined },
  ]);
  assert.deepEqual(prScopesForNodes([repoNode(NO_GITHUB_REPO)]), []);
  assert.deepEqual(prScopesForNodes([]), []);
});

test("prScopesForNodes collapses identical scopes to one `gh` call", () => {
  // The same repo node twice, and two nodes resolving to the same branch scope.
  const scopes = prScopesForNodes([
    repoNode(SCOPE_REPO),
    repoNode(SCOPE_REPO),
    wtNode(SCOPE_REPO, 1),
    wtNode(SCOPE_REPO, 1),
  ]);
  assert.deepEqual(scopes, [
    { kind: "repo", repo: REPO },
    { kind: "worktree", repo: REPO, branch: "a", badge: undefined, prNone: undefined },
  ]);
});

test("prScopesForNodes keeps a repo scope and its own worktree's branch scope apart", () => {
  // A repo scope usually subsumes its branches, but `gh pr list` caps at
  // PR_LIST_LIMIT — so both calls run and the *results* are deduped instead.
  const scopes = prScopesForNodes([repoNode(SCOPE_REPO), wtNode(SCOPE_REPO, 1)]);
  assert.equal(scopes.length, 2);
});

test("dedupePullRequests collapses a repo node and its worktree node's shared PR", () => {
  const other: PullRequest = { ...PR, number: 1300, url: "https://github.com/o/r/pull/1300" };
  // The repo scope found both PRs; the worktree scope found one of them again.
  assert.deepEqual(dedupePullRequests([PR, other, { ...PR }]), [PR, other]);
});

test("dedupePullRequests preserves first-seen order and tolerates an empty list", () => {
  assert.deepEqual(dedupePullRequests([]), []);
  assert.deepEqual(dedupePullRequests([PR]), [PR]);
});

test("scopeLabel names a repo scope and a branch scope", () => {
  assert.equal(scopeLabel({ kind: "repo", repo: REPO }), "rust-works/omni-dev");
  assert.equal(
    scopeLabel({ kind: "worktree", repo: REPO, branch: "issue-1357" }),
    "rust-works/omni-dev@issue-1357",
  );
  // A detached worktree has no branch to qualify the repo with.
  assert.equal(scopeLabel({ kind: "worktree", repo: REPO }), "rust-works/omni-dev");
});
