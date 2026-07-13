// Unit tests for the pure PR-discovery module. Nothing here imports `vscode`,
// so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  PR_BADGE_JSON_FIELDS,
  PR_JSON_FIELDS,
  PR_LIST_LIMIT,
  PullRequest,
  discoverPullRequests,
  parsePrList,
  prBadgeForBranch,
  prBadgeListArgs,
  prListArgsForBranch,
  prListArgsForRepo,
  prOverviewUri,
  prQuickPickDescription,
  prQuickPickLabel,
  rollupCheckState,
} from "./github";
import { TreeGithubIdentity } from "./tree";

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

test("prListArgsForBranch scopes to the head branch with --head", () => {
  assert.deepEqual(prListArgsForBranch(REPO, "issue-1300"), [
    "pr",
    "list",
    "--repo",
    "rust-works/omni-dev",
    "--head",
    "issue-1300",
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

test("discoverPullRequests lists all PRs for a repo scope", async () => {
  const calls: string[][] = [];
  const runner = async (args: string[]) => {
    calls.push(args);
    return JSON.stringify([PR]);
  };
  const prs = await discoverPullRequests({ kind: "repo", repo: REPO }, runner);
  assert.equal(prs.length, 1);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0], prListArgsForRepo(REPO));
});

test("discoverPullRequests scopes a worktree to its --head branch", async () => {
  const calls: string[][] = [];
  const runner = async (args: string[]) => {
    calls.push(args);
    return "[]";
  };
  const prs = await discoverPullRequests(
    { kind: "worktree", repo: REPO, branch: "issue-1300" },
    runner,
  );
  assert.deepEqual(prs, []);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0], prListArgsForBranch(REPO, "issue-1300"));
});

test("discoverPullRequests returns [] for a detached worktree without calling gh", async () => {
  let called = false;
  const runner = async (_args: string[]) => {
    called = true;
    return JSON.stringify([PR]);
  };
  const prs = await discoverPullRequests({ kind: "worktree", repo: REPO }, runner);
  assert.deepEqual(prs, []);
  assert.equal(called, false);
});

// --- PR badge (#1296): checks rollup + branch matching -----------------------

test("prBadgeListArgs lists all open PRs with the statusCheckRollup field", () => {
  assert.deepEqual(prBadgeListArgs(REPO), [
    "pr",
    "list",
    "--repo",
    "rust-works/omni-dev",
    "--state",
    "open",
    "--json",
    PR_BADGE_JSON_FIELDS,
    "--limit",
    PR_LIST_LIMIT,
  ]);
  // The badge field set is the base set plus the checks rollup.
  assert.ok(PR_BADGE_JSON_FIELDS.startsWith(`${PR_JSON_FIELDS},`));
  assert.match(PR_BADGE_JSON_FIELDS, /statusCheckRollup/);
});

test("rollupCheckState reports none for an empty or absent rollup", () => {
  assert.equal(rollupCheckState(undefined), "none");
  assert.equal(rollupCheckState([]), "none");
});

test("rollupCheckState maps a completed CheckRun's conclusion", () => {
  assert.equal(rollupCheckState([{ status: "COMPLETED", conclusion: "SUCCESS" }]), "success");
  assert.equal(rollupCheckState([{ status: "COMPLETED", conclusion: "FAILURE" }]), "failure");
  assert.equal(rollupCheckState([{ status: "COMPLETED", conclusion: "CANCELLED" }]), "failure");
  // Neutral/skipped are non-blocking → success.
  assert.equal(rollupCheckState([{ status: "COMPLETED", conclusion: "NEUTRAL" }]), "success");
  assert.equal(rollupCheckState([{ status: "COMPLETED", conclusion: "SKIPPED" }]), "success");
});

test("rollupCheckState treats an incomplete CheckRun as pending", () => {
  assert.equal(rollupCheckState([{ status: "IN_PROGRESS" }]), "pending");
  assert.equal(rollupCheckState([{ status: "QUEUED" }]), "pending");
});

test("rollupCheckState reads a StatusContext's state", () => {
  assert.equal(rollupCheckState([{ __typename: "StatusContext", state: "SUCCESS" }]), "success");
  assert.equal(rollupCheckState([{ __typename: "StatusContext", state: "FAILURE" }]), "failure");
  assert.equal(rollupCheckState([{ __typename: "StatusContext", state: "ERROR" }]), "failure");
  assert.equal(rollupCheckState([{ __typename: "StatusContext", state: "PENDING" }]), "pending");
});

test("rollupCheckState precedence: any failure dominates, else any pending, else success", () => {
  assert.equal(
    rollupCheckState([
      { status: "COMPLETED", conclusion: "SUCCESS" },
      { status: "IN_PROGRESS" },
      { status: "COMPLETED", conclusion: "FAILURE" },
    ]),
    "failure",
  );
  assert.equal(
    rollupCheckState([{ status: "COMPLETED", conclusion: "SUCCESS" }, { status: "IN_PROGRESS" }]),
    "pending",
  );
  assert.equal(
    rollupCheckState([
      { status: "COMPLETED", conclusion: "SUCCESS" },
      { __typename: "StatusContext", state: "SUCCESS" },
    ]),
    "success",
  );
});

test("prBadgeForBranch matches the head branch and rolls up its checks", () => {
  const prs: PullRequest[] = [
    {
      ...PR,
      number: 1300,
      headRefName: "issue-1300",
      isDraft: true,
      url: "https://github.com/rust-works/omni-dev/pull/1300",
      statusCheckRollup: [{ status: "COMPLETED", conclusion: "FAILURE" }],
    },
    { ...PR },
  ];
  assert.deepEqual(prBadgeForBranch(prs, "issue-1300"), {
    number: 1300,
    isDraft: true,
    checks: "failure",
    url: "https://github.com/rust-works/omni-dev/pull/1300",
  });
});

test("prBadgeForBranch returns undefined for no branch or no head match", () => {
  assert.equal(prBadgeForBranch([PR], undefined), undefined);
  assert.equal(prBadgeForBranch([PR], ""), undefined);
  assert.equal(prBadgeForBranch([PR], "no-such-branch"), undefined);
});

test("prBadgeForBranch takes the first head match and defaults absent checks to none", () => {
  // The PR fixture carries no statusCheckRollup → checks resolve to `none`.
  assert.deepEqual(prBadgeForBranch([PR], PR.headRefName), {
    number: PR.number,
    isDraft: false,
    checks: "none",
    url: PR.url,
  });
});
