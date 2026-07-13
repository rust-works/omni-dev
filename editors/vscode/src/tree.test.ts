// Unit tests for the pure repo/worktree tree model and formatters. Nothing here
// imports `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  PrBadge,
  TreeRepoPayload,
  TreeWorktreePayload,
  checkStateDecoration,
  isCurrentWindow,
  nodeId,
  repoLabel,
  reposToNodes,
  withAheadBehind,
  withPr,
  worktreeCheckDecoration,
  worktreeContextValue,
  worktreeDescription,
  worktreeLabel,
  worktreeNodes,
  worktreePrBadge,
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

test("withAheadBehind folds lazily-fetched counts in, and no-ops when absent", () => {
  const base: TreeWorktreePayload = { path: "/x", branch: "main", is_main: true, open: true };
  // Counts fetched via the ahead-behind op → a new payload carrying them, so the
  // description renders exactly as an eager snapshot would have (#1306).
  const merged = withAheadBehind(base, { ahead: 2, behind: 1 });
  assert.equal(worktreeDescription(merged), "↑2 ↓1");
  // The original is not mutated (a fresh object is returned).
  assert.equal(base.ahead, undefined);
  // No entry (undefined) or an empty entry (no upstream) leaves the worktree
  // untouched — same reference, renders without a sync indicator.
  assert.equal(withAheadBehind(base, undefined), base);
  assert.equal(withAheadBehind(base, {}), base);
  assert.equal(worktreeDescription(withAheadBehind(base, {})), "");
  // A one-sided result is still applied.
  assert.equal(worktreeDescription(withAheadBehind(base, { ahead: 4 })), "↑4");
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

test("worktreeContextValue appends `.github` only when the parent repo is on GitHub", () => {
  // hasGithub defaults to false → the existing values are byte-for-byte unchanged.
  assert.equal(worktreeContextValue(REPOS[0].worktrees[0], "w1", false), "worktree.current.main");
  assert.equal(worktreeContextValue(REPOS[0].worktrees[1], "w1", false), "worktree.linked");

  // hasGithub true → a trailing `.github` segment on top of the existing value.
  assert.equal(
    worktreeContextValue(REPOS[0].worktrees[0], "w1", true),
    "worktree.current.main.github",
  );
  assert.equal(
    worktreeContextValue(REPOS[0].worktrees[0], "w2", true),
    "worktree.open.main.github",
  );
  assert.equal(worktreeContextValue(REPOS[0].worktrees[1], "w1", true), "worktree.linked.github");

  const closedMain = { path: "/repo", is_main: true, open: false };
  assert.equal(worktreeContextValue(closedMain, undefined, true), "worktree.main.github");

  // The `.github` suffix leaves the (unanchored) close-menu regexes matching: a
  // main tree with a window still matches Close Window, a linked one Close Worktree.
  assert.match(worktreeContextValue(REPOS[0].worktrees[0], "w1", true), /worktree\.(current|open)\.main/);
  assert.match(worktreeContextValue(REPOS[0].worktrees[1], "w1", true), /worktree\..*linked/);
  // …and both GitHub variants match the new Open-PR menu's `/github/` gate.
  assert.match(worktreeContextValue(REPOS[0].worktrees[0], "w1", true), /github/);
  assert.match(worktreeContextValue(REPOS[0].worktrees[1], "w1", true), /github/);
});

// --- PR badge (#1296) --------------------------------------------------------

const OPEN_PR: PrBadge = {
  number: 65,
  isDraft: false,
  checks: "success",
  url: "https://github.com/rust-works/omni-dev/pull/65",
};

test("worktreePrBadge formats the number and draft marker, without a checks glyph", () => {
  const wt = REPOS[0].worktrees[0];
  // The check verdict no longer appears in the badge text — it is a colored file
  // decoration since #1324 — so every check state renders the same badge.
  assert.equal(worktreePrBadge({ ...wt, pr: OPEN_PR }), "#65");
  assert.equal(worktreePrBadge({ ...wt, pr: { ...OPEN_PR, checks: "failure" } }), "#65");
  assert.equal(worktreePrBadge({ ...wt, pr: { ...OPEN_PR, checks: "pending" } }), "#65");
  assert.equal(worktreePrBadge({ ...wt, pr: { ...OPEN_PR, checks: "none" } }), "#65");
  // Draft → a `draft` marker, still with no glyph, whatever the check state.
  assert.equal(worktreePrBadge({ ...wt, pr: { ...OPEN_PR, isDraft: true } }), "#65 draft");
  assert.equal(
    worktreePrBadge({ ...wt, pr: { ...OPEN_PR, isDraft: true, checks: "failure" } }),
    "#65 draft",
  );
  // No PR → empty.
  assert.equal(worktreePrBadge(wt), "");
});

test("worktreeCheckDecoration maps each PR check state to a colored badge (#1324)", () => {
  const wt = REPOS[0].worktrees[0];
  assert.deepEqual(worktreeCheckDecoration({ ...wt, pr: OPEN_PR }), {
    badge: "✓",
    colorId: "charts.green",
    tooltip: "checks passing",
  });
  assert.deepEqual(worktreeCheckDecoration({ ...wt, pr: { ...OPEN_PR, checks: "failure" } }), {
    badge: "✗",
    colorId: "charts.red",
    tooltip: "checks failing",
  });
  assert.deepEqual(worktreeCheckDecoration({ ...wt, pr: { ...OPEN_PR, checks: "pending" } }), {
    badge: "●",
    colorId: "charts.yellow",
    tooltip: "checks pending",
  });
  // A PR with no checks configured → no badge.
  assert.equal(worktreeCheckDecoration({ ...wt, pr: { ...OPEN_PR, checks: "none" } }), undefined);
  // No PR at all → no badge.
  assert.equal(worktreeCheckDecoration(wt), undefined);
});

test("checkStateDecoration maps a bare check state, undefined for none", () => {
  assert.deepEqual(checkStateDecoration("success"), {
    badge: "✓",
    colorId: "charts.green",
    tooltip: "checks passing",
  });
  assert.equal(checkStateDecoration("failure")?.badge, "✗");
  assert.equal(checkStateDecoration("failure")?.colorId, "charts.red");
  assert.equal(checkStateDecoration("pending")?.tooltip, "checks pending");
  assert.equal(checkStateDecoration("none"), undefined);
});

test("withPr folds a badge in, and no-ops when absent", () => {
  const base = REPOS[0].worktrees[0]; // carries no pr
  const merged = withPr(base, OPEN_PR);
  assert.deepEqual(merged.pr, OPEN_PR);
  // The original is not mutated (a fresh object is returned).
  assert.equal(base.pr, undefined);
  // An absent badge leaves the worktree untouched — same reference.
  assert.equal(withPr(base, undefined), base);
});

test("worktreeDescription shows sync and PR together, each only when present", () => {
  // Sync only (no PR) — byte-for-byte the pre-#1296 behavior.
  assert.equal(worktreeDescription(REPOS[0].worktrees[0]), "↑2 ↓0");
  // Sync + PR, separated by a gap. The check verdict is a colored badge (#1324),
  // not part of the description text.
  assert.equal(worktreeDescription({ ...REPOS[0].worktrees[0], pr: OPEN_PR }), "↑2 ↓0  #65");
  // PR only (no upstream counts).
  assert.equal(
    worktreeDescription({ path: "/x", is_main: true, open: false, pr: OPEN_PR }),
    "#65",
  );
  // Neither → empty.
  assert.equal(worktreeDescription({ path: "/x", is_main: true, open: false }), "");
});

test("worktreeTooltip adds a PR line only when a PR is resolved", () => {
  const withPrTip = worktreeTooltip({ ...REPOS[0].worktrees[0], pr: OPEN_PR }, REPOS[0], "w1");
  assert.match(withPrTip, /PR #65 · open · checks passing/);
  // The branch line keeps only the sync counts — the PR is not duplicated there.
  assert.match(withPrTip, /main {2}↑2 ↓0/);

  const draftFailing = worktreeTooltip(
    { ...REPOS[0].worktrees[0], pr: { ...OPEN_PR, isDraft: true, checks: "failure" } },
    REPOS[0],
  );
  assert.match(draftFailing, /PR #65 · draft · checks failing/);

  // A PR with no checks omits the checks clause.
  const noChecks = worktreeTooltip(
    { ...REPOS[0].worktrees[0], pr: { ...OPEN_PR, checks: "none" } },
    REPOS[0],
  );
  assert.match(noChecks, /PR #65 · open$/m);
  assert.doesNotMatch(noChecks, /checks/);

  // No PR → no PR line at all.
  assert.doesNotMatch(worktreeTooltip(REPOS[0].worktrees[0], REPOS[0]), /PR #/);
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
