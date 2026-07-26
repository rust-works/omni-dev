// Unit tests for the pure row-icon layer. Nothing here imports `vscode`, so it runs
// under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { repoRowIcon, worktreeRowIcon } from "./icons";
import { TreeRepoPayload, TreeWorktreePayload } from "./tree";

const GITHUB_REPO: TreeRepoPayload = {
  main_repo: "omni-dev",
  github: { owner: "rust-works", name: "omni-dev" },
  root: "/Users/jky/wrk/rust-works/omni-dev",
  worktrees: [],
};

const PLAIN_REPO: TreeRepoPayload = {
  main_repo: "scratch",
  root: "/Users/jky/wrk/scratch",
  worktrees: [],
};

const CLOSED_WT: TreeWorktreePayload = {
  path: "/Users/jky/wrk/work-trees/omni-dev/issue-1428",
  branch: "issue-1428",
  is_main: false,
  open: false,
};

const OPEN_WT: TreeWorktreePayload = { ...CLOSED_WT, open: true, window_key: "w2" };
const SELF_WT: TreeWorktreePayload = { ...CLOSED_WT, open: true, window_key: "w1" };

test("a repo row's icon tracks GitHub identity and PR-poll state", () => {
  // GitHub origin, polling on and the master switch on: green.
  assert.deepEqual(repoRowIcon({ ...GITHUB_REPO, polling_enabled: true }, true), {
    iconId: "github",
    colorId: "charts.green",
  });
  // The same repo with the master switch off greys, as does one not being polled.
  assert.deepEqual(repoRowIcon({ ...GITHUB_REPO, polling_enabled: true }, false), {
    iconId: "github",
    colorId: undefined,
  });
  assert.deepEqual(repoRowIcon(GITHUB_REPO, true), { iconId: "github", colorId: undefined });
  // A non-GitHub repo keeps the plain glyph and no colour.
  assert.deepEqual(repoRowIcon(PLAIN_REPO, true), { iconId: "repo" });
});

test("a worktree row's icon is the three-way open badge", () => {
  assert.deepEqual(worktreeRowIcon(SELF_WT, "w1"), { iconId: "check", colorId: "charts.blue" });
  assert.deepEqual(worktreeRowIcon(OPEN_WT, "w1"), {
    iconId: "circle-filled",
    colorId: "charts.green",
  });
  assert.deepEqual(worktreeRowIcon(CLOSED_WT, "w1"), { iconId: "git-branch" });
  // No window key at all: this window owns nothing, so an open worktree is "elsewhere".
  assert.deepEqual(worktreeRowIcon(OPEN_WT, undefined), {
    iconId: "circle-filled",
    colorId: "charts.green",
  });
});

test("the mid-rebase cue takes the worktree icon over from open state", () => {
  assert.deepEqual(worktreeRowIcon({ ...OPEN_WT, rebasing: true }, "w1"), {
    iconId: "sync~spin",
    colorId: "charts.yellow",
  });
  // The durable half — a conflict left in place — wins on the current-window row too.
  assert.deepEqual(worktreeRowIcon({ ...SELF_WT, operation: "rebase-merge" }, "w1"), {
    iconId: "warning",
    colorId: "charts.yellow",
  });
});
