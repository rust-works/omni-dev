// Unit tests for the pure "Copy PR URL" clipboard model (#1430). Nothing here
// imports `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { PullRequest, prScopeForNode, prScopeKey } from "./github";
import {
  PrLookup,
  prClipboardLines,
  prClipboardText,
  prCopySummary,
  prUrlCount,
  prUrlsText,
} from "./prClipboard";
import { Node, TreeGithubIdentity, TreeRepoPayload } from "./tree";

const GITHUB: TreeGithubIdentity = { owner: "rust-works", name: "omni-dev" };

const url = (n: number) => `https://github.com/rust-works/omni-dev/pull/${n}`;

const pr = (n: number, headRefName: string): PullRequest => ({
  number: n,
  title: `PR ${n}`,
  url: url(n),
  headRefName,
  baseRefName: "main",
  isDraft: false,
  state: "OPEN",
});

const REPO: TreeRepoPayload = {
  main_repo: "omni-dev",
  github: GITHUB,
  root: "/home/me/omni-dev",
  worktrees: [
    { path: "/home/me/omni-dev", branch: "main", is_main: true, open: true },
    { path: "/home/me/wt/a", branch: "issue-1417", is_main: false, open: true },
    { path: "/home/me/wt/b", branch: "issue-1428", is_main: false, open: true },
    { path: "/home/me/wt/detached", is_main: false, open: true },
  ],
};

/** A repo with no `github` — an origin that is not `github.com`, or none at all. */
const NO_GITHUB: TreeRepoPayload = {
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

/** Builds the scope-keyed lookup map the way the command's handler does. */
function lookups(entries: Array<[Node, PrLookup]>): Map<string, PrLookup> {
  const map = new Map<string, PrLookup>();
  for (const [node, lookup] of entries) {
    const scope = prScopeForNode(node);
    if (scope) {
      map.set(prScopeKey(scope), lookup);
    }
  }
  return map;
}

const ok = (prs: PullRequest[]): PrLookup => ({ status: "ok", prs });
const failed: PrLookup = { status: "failed" };

test("a single worktree row with an open PR copies exactly its URL", () => {
  const node = wtNode(REPO, 1);
  const lines = prClipboardLines([node], lookups([[node, ok([pr(1417, "issue-1417")])]]));
  assert.deepEqual(lines, [url(1417)]);
  assert.equal(prClipboardText(lines), url(1417));
});

test("lines follow selection order, one per row, with no row dropped", () => {
  const withPr = wtNode(REPO, 1);
  const without = wtNode(REPO, 2);
  const nodes = [withPr, without, repoNode(REPO)];
  const lines = prClipboardLines(
    nodes,
    lookups([
      [withPr, ok([pr(1417, "issue-1417")])],
      [without, ok([])],
      [nodes[2], ok([pr(1415, "issue-1415")])],
    ]),
  );
  assert.deepEqual(lines, [
    url(1417),
    "# No PR for issue-1428 in /home/me/wt/b",
    url(1415),
  ]);
});

test("a PR-less worktree names its branch and absolute path", () => {
  const node = wtNode(REPO, 2);
  assert.deepEqual(prClipboardLines([node], lookups([[node, ok([])]])), [
    "# No PR for issue-1428 in /home/me/wt/b",
  ]);
});

test("a detached worktree is a placeholder, not an error", () => {
  const node = wtNode(REPO, 3);
  assert.deepEqual(prClipboardLines([node], lookups([[node, ok([])]])), [
    "# No PR for (detached) in /home/me/wt/detached",
  ]);
});

test("a non-GitHub row has no scope at all and still gets a placeholder", () => {
  const wt = wtNode(NO_GITHUB, 0);
  const repo = repoNode(NO_GITHUB);
  // Neither node yields a scope, so the map is empty — the join must not throw
  // and must not treat the absence as a failure.
  assert.deepEqual(prClipboardLines([wt, repo], new Map()), [
    "# No PR for main in /home/me/scratch",
    "# No open PRs for scratch",
  ]);
});

test("a repo row lists every open PR, one per line", () => {
  const node = repoNode(REPO);
  const lines = prClipboardLines(
    [node],
    lookups([[node, ok([pr(1417, "issue-1417"), pr(1415, "issue-1415")])]]),
  );
  assert.deepEqual(lines, [url(1417), url(1415)]);
});

test("a repo row with no open PRs says so by owner/name", () => {
  const node = repoNode(REPO);
  assert.deepEqual(prClipboardLines([node], lookups([[node, ok([])]])), [
    "# No open PRs for rust-works/omni-dev",
  ]);
});

test("a failed lookup is distinct from no PR, and the rest of the batch still copies", () => {
  const bad = wtNode(REPO, 1);
  const good = wtNode(REPO, 2);
  const lines = prClipboardLines(
    [bad, good],
    lookups([
      [bad, failed],
      [good, ok([pr(1428, "issue-1428")])],
    ]),
  );
  assert.deepEqual(lines, [
    "# PR lookup failed for issue-1417 in /home/me/wt/a",
    url(1428),
  ]);
});

test("a failed repo lookup names the repo", () => {
  const node = repoNode(REPO);
  assert.deepEqual(prClipboardLines([node], lookups([[node, failed]])), [
    "# PR lookup failed for rust-works/omni-dev",
  ]);
});

test("URLs de-duplicate across the whole block", () => {
  const repo = repoNode(REPO);
  const wt = wtNode(REPO, 1);
  const shared = pr(1417, "issue-1417");
  const lines = prClipboardLines(
    [repo, wt],
    lookups([
      [repo, ok([shared, pr(1415, "issue-1415")])],
      [wt, ok([shared])],
    ]),
  );
  // The worktree contributes no second copy of #1417 — and no placeholder
  // either, since claiming it has no PR would be false.
  assert.deepEqual(lines, [url(1417), url(1415)]);
});

test("placeholders never de-duplicate", () => {
  const a = wtNode(REPO, 2);
  const b = wtNode(REPO, 3);
  const lines = prClipboardLines(
    [a, b],
    lookups([
      [a, ok([])],
      [b, ok([])],
    ]),
  );
  assert.deepEqual(lines, [
    "# No PR for issue-1428 in /home/me/wt/b",
    "# No PR for (detached) in /home/me/wt/detached",
  ]);
});

test("two rows sharing one failed scope each get their own line", () => {
  // Same branch checked out twice: one scope, one `gh` call, two rows.
  const twin: TreeRepoPayload = {
    ...REPO,
    worktrees: [
      { path: "/home/me/wt/x", branch: "shared", is_main: false, open: true },
      { path: "/home/me/wt/y", branch: "shared", is_main: false, open: true },
    ],
  };
  const a = wtNode(twin, 0);
  const b = wtNode(twin, 1);
  assert.deepEqual(prClipboardLines([a, b], lookups([[a, failed]])), [
    "# PR lookup failed for shared in /home/me/wt/x",
    "# PR lookup failed for shared in /home/me/wt/y",
  ]);
});

test("an empty selection is an empty block", () => {
  assert.deepEqual(prClipboardLines([], new Map()), []);
  assert.equal(prClipboardText([]), "");
});

test("prClipboardText joins with newlines and adds no trailing one", () => {
  assert.equal(prClipboardText([url(1417), "# No PR for x in /y"]), `${url(1417)}\n# No PR for x in /y`);
});

test("prUrlsText de-duplicates and joins already-resolved PRs, with no placeholders", () => {
  const shared = pr(1417, "issue-1417");
  assert.equal(
    prUrlsText([shared, pr(1415, "issue-1415"), shared]),
    `${url(1417)}\n${url(1415)}`,
  );
  assert.equal(prUrlsText([]), "");
});

test("prUrlCount counts URLs, not placeholders", () => {
  assert.equal(prUrlCount([url(1417), "# No PR for x in /y", url(1415)]), 2);
  assert.equal(prUrlCount(["# No open PRs for o/r"]), 0);
});

test("prCopySummary reports placeholders out loud rather than as URLs", () => {
  assert.equal(prCopySummary([url(1417)]), "Copied 1 PR URL");
  assert.equal(prCopySummary([url(1417), url(1415)]), "Copied 2 PR URLs");
  assert.equal(
    prCopySummary([url(1417), "# No PR for x in /y"]),
    "Copied 1 PR URL and 1 placeholder",
  );
  assert.equal(
    prCopySummary([url(1417), "# No PR for x in /y", "# No PR for z in /w"]),
    "Copied 1 PR URL and 2 placeholders",
  );
  assert.equal(prCopySummary(["# No open PRs for o/r"]), "Copied 1 placeholder");
});
