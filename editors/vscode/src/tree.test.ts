// Unit tests for the pure repo/worktree tree model and formatters. Nothing here
// imports `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  TreeRepoPayload,
  nodeId,
  repoLabel,
  reposToNodes,
  worktreeContextValue,
  worktreeDescription,
  worktreeLabel,
  worktreeNodes,
  worktreeTooltip,
} from "./tree";

// A representative snapshot: a GitHub repo with an open main worktree (ahead) and
// a closed linked worktree (ahead+behind), plus a non-GitHub repo whose single
// worktree is detached with no upstream.
const REPOS: TreeRepoPayload[] = [
  {
    main_repo: "omni-dev",
    github: { owner: "rust-works", name: "omni-dev" },
    root: "/home/me/omni-dev",
    worktrees: [
      {
        path: "/home/me/omni-dev",
        branch: "main",
        ahead: 2,
        behind: 0,
        is_main: true,
        open: true,
        window_key: "w1",
      },
      {
        path: "/home/me/wt/issue-1300",
        branch: "issue-1300",
        ahead: 1,
        behind: 3,
        is_main: false,
        open: false,
      },
    ],
  },
  {
    main_repo: "scratch",
    root: "/home/me/scratch",
    worktrees: [{ path: "/home/me/scratch", is_main: true, open: false }],
  },
];

test("reposToNodes yields one repo node per repo, in order", () => {
  const nodes = reposToNodes(REPOS);
  assert.equal(nodes.length, 2);
  assert.deepEqual(
    nodes.map((n) => (n.kind === "repo" ? n.repo.main_repo : "?")),
    ["omni-dev", "scratch"],
  );
});

test("worktreeNodes yields child nodes carrying their parent repo", () => {
  const nodes = worktreeNodes(REPOS[0]);
  assert.equal(nodes.length, 2);
  for (const n of nodes) {
    assert.equal(n.kind, "worktree");
    if (n.kind === "worktree") {
      assert.equal(n.repo.main_repo, "omni-dev");
    }
  }
});

test("repoLabel prefers the GitHub owner/name, else the main repo name", () => {
  assert.equal(repoLabel(REPOS[0]), "rust-works/omni-dev");
  assert.equal(repoLabel(REPOS[1]), "scratch");
});

test("worktreeLabel is the branch, or the folder basename when detached", () => {
  assert.equal(worktreeLabel(REPOS[0].worktrees[0]), "main");
  assert.equal(worktreeLabel(REPOS[1].worktrees[0]), "scratch");
});

test("worktreeDescription formats the sync counts, omitting absent sides", () => {
  assert.equal(worktreeDescription(REPOS[0].worktrees[0]), "↑2 ↓0");
  assert.equal(worktreeDescription(REPOS[0].worktrees[1]), "↑1 ↓3");
  // No upstream → both counts absent → empty description.
  assert.equal(worktreeDescription(REPOS[1].worktrees[0]), "");
  // One-sided (defensive): only the present side is shown.
  assert.equal(worktreeDescription({ path: "/x", is_main: true, open: false, ahead: 4 }), "↑4");
  assert.equal(worktreeDescription({ path: "/x", is_main: true, open: false, behind: 5 }), "↓5");
});

test("worktreeContextValue marks the open badge under a shared `worktree` prefix", () => {
  assert.equal(worktreeContextValue(REPOS[0].worktrees[0]), "worktree.open");
  assert.equal(worktreeContextValue(REPOS[0].worktrees[1]), "worktree");
});

test("nodeId is stable and distinguishes repos from worktrees", () => {
  assert.equal(nodeId({ kind: "repo", repo: REPOS[0] }), "repo:/home/me/omni-dev");
  assert.equal(
    nodeId({ kind: "worktree", repo: REPOS[0], wt: REPOS[0].worktrees[0] }),
    "wt:/home/me/omni-dev",
  );
});

test("worktreeTooltip carries path, kind, parent repo, sync, and open state", () => {
  const tip = worktreeTooltip(REPOS[0].worktrees[0], REPOS[0]);
  assert.match(tip, /\/home\/me\/omni-dev/);
  assert.match(tip, /main working tree of rust-works\/omni-dev/);
  assert.match(tip, /main {2}↑2 ↓0/);
  assert.match(tip, /● window open/);

  const closed = worktreeTooltip(REPOS[0].worktrees[1], REPOS[0]);
  assert.match(closed, /linked worktree of rust-works\/omni-dev/);
  assert.match(closed, /no window open/);
});
