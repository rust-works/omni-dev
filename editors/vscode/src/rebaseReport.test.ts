// Unit tests for the pure rebase reporting model. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  confirmDetail,
  confirmTitle,
  failedFetches,
  nothingToRebaseMessage,
  outcomeLabel,
  pendingOutcomes,
  skipReasonText,
  skippedOutcomes,
  summarize,
  upToDateCount,
} from "./rebaseReport";
import { RebaseOutcome, RebaseReply } from "./socket";

function outcome(partial: Partial<RebaseOutcome> & { status: string }): RebaseOutcome {
  return {
    path: "/w/feature",
    branch: "feature",
    onto: "origin/main",
    ...partial,
  };
}

test("pendingOutcomes selects only the worktrees that would be rebased", () => {
  const reply: RebaseReply = {
    worktrees: [
      outcome({ status: "would-rebase", branch: "a", behind: 2 }),
      outcome({ status: "up-to-date", branch: "b" }),
      outcome({ status: "skipped", branch: "c", reason: "dirty" }),
    ],
  };
  assert.deepEqual(
    pendingOutcomes(reply).map((p) => p.branch),
    ["a"],
  );
  // Up-to-date is dropped from the skip list: it is the expected case, and
  // listing it would bury the reasons that actually need the user.
  assert.deepEqual(
    skippedOutcomes(reply).map((s) => s.branch),
    ["c"],
  );
  assert.equal(upToDateCount(reply), 1);
});

test("an empty reply degrades to empty lists rather than throwing", () => {
  assert.deepEqual(pendingOutcomes({}), []);
  assert.deepEqual(skippedOutcomes({}), []);
  assert.deepEqual(failedFetches({}), []);
  assert.equal(upToDateCount({}), 0);
});

test("outcomeLabel prefers the branch and falls back to the folder basename", () => {
  assert.equal(outcomeLabel(outcome({ status: "rebased", branch: "feature" })), "feature");
  assert.equal(
    outcomeLabel(outcome({ status: "skipped", branch: undefined, path: "/w/issue-1415/" })),
    "issue-1415",
  );
  assert.equal(
    outcomeLabel(outcome({ status: "skipped", branch: undefined, path: "C:\\w\\wt" })),
    "wt",
  );
});

test("skipReasonText spells out each slug and passes an unknown one through", () => {
  assert.equal(skipReasonText("dirty"), "uncommitted changes");
  assert.equal(skipReasonText("operation-in-progress"), "a rebase or merge already in progress");
  assert.equal(skipReasonText("detached-head"), "detached HEAD");
  assert.equal(skipReasonText("not-a-worktree"), "not a git worktree");
  assert.equal(skipReasonText("no-onto-ref"), "no resolvable target branch");
  // A slug from a newer daemon still renders as itself rather than vanishing.
  assert.equal(skipReasonText("something-new"), "something-new");
  // "main-working-tree" (#1438, ADR-0060): the daemon no longer sends it, but an
  // old, not-yet-restarted one might — falls through here rather than vanishing.
  assert.equal(skipReasonText("main-working-tree"), "main-working-tree");
  assert.equal(skipReasonText(undefined), "skipped");
});

test("confirmDetail lists real behind-counts and the skipped worktrees", () => {
  const detail = confirmDetail(
    [
      outcome({ status: "would-rebase", branch: "a", behind: 3 }),
      outcome({ status: "would-rebase", branch: "b" }),
    ],
    [outcome({ status: "skipped", branch: "c", reason: "dirty" })],
  );
  // The counts are the payoff of checking before confirming (#1409 could not
  // show them, since the tree's `behind` measures a different thing).
  assert.match(detail, /• a \(3 behind\)/);
  assert.match(detail, /• b$/m, "an absent count renders no parenthetical");
  assert.match(detail, /Skipped:/);
  assert.match(detail, /• c — uncommitted changes/);
  assert.match(detail, /git rebase --continue/);
});

test("confirmDetail omits the skipped section when nothing was skipped", () => {
  const detail = confirmDetail([outcome({ status: "would-rebase", behind: 1 })], []);
  assert.doesNotMatch(detail, /Skipped:/);
});

test("confirmTitle names a single worktree and counts a partial batch", () => {
  assert.equal(
    confirmTitle([outcome({ status: "would-rebase", branch: "a" })], 1),
    "Rebase “a” onto the remote default branch?",
  );
  const two = [
    outcome({ status: "would-rebase", branch: "a" }),
    outcome({ status: "would-rebase", branch: "b" }),
  ];
  assert.equal(confirmTitle(two, 2), "Rebase 2 worktrees onto the remote default branch?");
  assert.equal(confirmTitle(two, 5), "Rebase 2 of 5 worktrees onto the remote default branch?");
});

test("nothingToRebaseMessage leads with a failed fetch", () => {
  const reply: RebaseReply = {
    fetches: [
      { repo_root: "/r", onto: "origin/main", fetched: true, ok: false, detail: "no route" },
    ],
    worktrees: [outcome({ status: "fetch-failed", detail: "the repository's fetch failed" })],
  };
  const message = nothingToRebaseMessage(reply, 1);
  assert.match(message, /could not fetch/);
  assert.match(message, /no route/);
});

test("nothingToRebaseMessage names the skip reasons, and up-to-date on its own", () => {
  assert.equal(
    nothingToRebaseMessage({ worktrees: [outcome({ status: "up-to-date" })] }, 1),
    "all already up to date",
  );
  const mixed: RebaseReply = {
    worktrees: [
      outcome({ status: "skipped", reason: "dirty" }),
      outcome({ status: "skipped", reason: "detached-head" }),
      outcome({ status: "up-to-date" }),
    ],
  };
  const message = nothingToRebaseMessage(mixed, 3);
  assert.match(message, /2 of 3 skipped \(uncommitted changes, detached HEAD\)/);
  assert.match(message, /1 already up to date/);
  // Nothing at all still says something rather than rendering blank.
  assert.equal(nothingToRebaseMessage({}, 0), "nothing to rebase");
});

test("summarize reports a clean batch as info", () => {
  const summary = summarize({
    worktrees: [
      outcome({ status: "rebased", behind: 1 }),
      outcome({ status: "rebased", behind: 2 }),
    ],
  });
  assert.equal(summary.severity, "info");
  assert.equal(summary.message, "rebased 2 worktrees");
});

test("summarize warns and names the worktrees left mid-rebase", () => {
  const summary = summarize({
    worktrees: [
      outcome({ status: "rebased", branch: "a" }),
      outcome({ status: "conflict", branch: "b", left_in_place: true, detail: "CONFLICT" }),
    ],
  });
  // Left-in-place is a warning: the batch succeeded, but a worktree is now
  // sitting mid-rebase waiting for the user, so it is named and instructed.
  assert.equal(summary.severity, "warning");
  assert.match(summary.message, /rebased 1 worktree/);
  assert.match(summary.message, /1 left mid-rebase to resolve: b/);
  assert.match(summary.message, /git rebase --continue/);
});

test("summarize distinguishes a rolled-back conflict from a kept one", () => {
  const summary = summarize({
    worktrees: [outcome({ status: "conflict", branch: "b", left_in_place: false })],
  });
  assert.equal(summary.severity, "warning");
  assert.match(summary.message, /rolled back/);
  assert.doesNotMatch(summary.message, /git rebase --continue/);
});

test("summarize escalates a failed fetch to an error", () => {
  const summary = summarize({
    worktrees: [outcome({ status: "fetch-failed", detail: "no route" })],
  });
  assert.equal(summary.severity, "error");
  assert.match(summary.message, /failed fetch/);
});

test("summarize says so when nothing happened", () => {
  assert.deepEqual(summarize({}), { severity: "info", message: "nothing was rebased" });
});
