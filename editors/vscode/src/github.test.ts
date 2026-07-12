// Unit tests for the pure PR-discovery module. Nothing here imports `vscode`,
// so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  PR_JSON_FIELDS,
  PR_LIST_LIMIT,
  PullRequest,
  discoverPullRequests,
  parsePrList,
  prListArgsForBranch,
  prListArgsForRepo,
  prOverviewUri,
  prQuickPickDescription,
  prQuickPickLabel,
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
