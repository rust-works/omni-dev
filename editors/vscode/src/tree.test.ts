// Unit tests for the pure repo/worktree tree model and formatters. Nothing here
// imports `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  TreeRepoPayload,
  isCurrentWindow,
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

test("worktreeNodes hides no-window worktrees when showClosed is false", () => {
  // Default / explicit-true: every worktree, byte-for-byte the current behavior.
  assert.equal(worktreeNodes(REPOS[0]).length, 2);
  assert.equal(worktreeNodes(REPOS[0], true).length, 2);

  // showClosed false: the closed linked worktree is dropped, the open one stays.
  const visible = worktreeNodes(REPOS[0], false);
  assert.equal(visible.length, 1);
  const node = visible[0];
  assert.equal(node.kind, "worktree");
  if (node.kind === "worktree") {
    assert.equal(node.wt.branch, "main");
    assert.equal(node.wt.open, true);
  }

  // A repo whose only worktree has no window keeps ≥1 open in practice (repos are
  // derived from open windows); as a pure function it can return an empty list,
  // but the daemon's invariant means the filter never empties a real repo.
  assert.equal(worktreeNodes(REPOS[1], false).length, 0);
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

test("isCurrentWindow matches only the open worktree whose key is this window's", () => {
  // Open here: the open worktree's key equals this window's key.
  assert.equal(isCurrentWindow(REPOS[0].worktrees[0], "w1"), true);
  // Open, but in another window (keys differ).
  assert.equal(isCurrentWindow(REPOS[0].worktrees[0], "w2"), false);
  // Closed worktree never matches, whatever the key.
  assert.equal(isCurrentWindow(REPOS[0].worktrees[1], "w1"), false);
  // An unknown window key (e.g. before assignment) never matches an open worktree.
  assert.equal(isCurrentWindow(REPOS[0].worktrees[0], undefined), false);
  // Degenerate: open with no key and an absent window key must not match.
  assert.equal(isCurrentWindow({ path: "/x", is_main: true, open: true }, undefined), false);
});

test("worktreeContextValue encodes open state and structural role under a shared `worktree` prefix", () => {
  // The main working tree (is_main) → a trailing `.main`, across open states.
  assert.equal(worktreeContextValue(REPOS[0].worktrees[0], "w1"), "worktree.current.main");
  assert.equal(worktreeContextValue(REPOS[0].worktrees[0], "w2"), "worktree.open.main");
  assert.equal(worktreeContextValue(REPOS[0].worktrees[0]), "worktree.open.main");
  // A linked worktree → a trailing `.linked`. The fixture's linked one is closed.
  assert.equal(worktreeContextValue(REPOS[0].worktrees[1], "w1"), "worktree.linked");
  assert.equal(worktreeContextValue(REPOS[0].worktrees[1]), "worktree.linked");

  // The current/open linked variants (not in the fixture) round out the six.
  const linkedHere = { path: "/wt/x", is_main: false, open: true, window_key: "w1" };
  const linkedElsewhere = { path: "/wt/y", is_main: false, open: true, window_key: "w9" };
  assert.equal(worktreeContextValue(linkedHere, "w1"), "worktree.current.linked");
  assert.equal(worktreeContextValue(linkedElsewhere, "w1"), "worktree.open.linked");

  // A closed main tree matches neither close menu (nothing to close or delete).
  const closedMain = { path: "/repo", is_main: true, open: false };
  assert.equal(worktreeContextValue(closedMain), "worktree.main");
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
  // No window key supplied → the generic open line, not the current-window one.
  assert.match(tip, /● window open/);

  const closed = worktreeTooltip(REPOS[0].worktrees[1], REPOS[0]);
  assert.match(closed, /linked worktree of rust-works\/omni-dev/);
  assert.match(closed, /no window open/);
});

test("worktreeTooltip distinguishes the current window's open line", () => {
  // Matching key → `● this window`.
  const here = worktreeTooltip(REPOS[0].worktrees[0], REPOS[0], "w1");
  assert.match(here, /● this window/);
  assert.doesNotMatch(here, /● window open/);

  // Non-matching key → the generic `● window open`.
  const elsewhere = worktreeTooltip(REPOS[0].worktrees[0], REPOS[0], "w2");
  assert.match(elsewhere, /● window open/);
  assert.doesNotMatch(elsewhere, /● this window/);
});
