// Unit tests for the pure push reporting model. Nothing here imports `vscode`, so
// it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  confirmDetail,
  confirmTitle,
  forcedOutcomes,
  nothingToPushMessage,
  outcomeLabel,
  pendingLine,
  pendingOutcomes,
  skipReasonText,
  skippedOutcomes,
  summarize,
  upToDateCount,
} from "./pushReport";
import { PushOutcome, PushReply } from "./socket";

function outcome(partial: Partial<PushOutcome> & { status: string }): PushOutcome {
  return {
    path: "/w/feature",
    branch: "feature",
    remote: "origin",
    remote_branch: "feature",
    ...partial,
  };
}

test("pendingOutcomes selects every flavour of push and nothing else", () => {
  const reply: PushReply = {
    worktrees: [
      outcome({ status: "would-force", branch: "a", ahead: 2, behind: 1 }),
      outcome({ status: "would-fast-forward", branch: "b", ahead: 3 }),
      outcome({ status: "would-create", branch: "c" }),
      outcome({ status: "up-to-date", branch: "d" }),
      outcome({ status: "skipped", branch: "e", reason: "detached-head" }),
    ],
  };
  assert.deepEqual(
    pendingOutcomes(reply).map((p) => p.branch),
    ["a", "b", "c"],
  );
  assert.deepEqual(
    forcedOutcomes(reply).map((p) => p.branch),
    ["a"],
    "only a diverged branch needs the lease",
  );
  assert.equal(upToDateCount(reply), 1);
});

test("skippedOutcomes drops the up-to-date rows so the reasons that matter show", () => {
  const reply: PushReply = {
    worktrees: [
      outcome({ status: "up-to-date", branch: "a" }),
      outcome({ status: "skipped", branch: "b", reason: "default-branch-force-push" }),
    ],
  };
  assert.deepEqual(
    skippedOutcomes(reply).map((s) => s.branch),
    ["b"],
  );
});

test("pendingLine distinguishes a force from a fast-forward at a glance", () => {
  const forced = pendingLine(
    outcome({ status: "would-force", branch: "feat", ahead: 2, behind: 3 }),
  );
  assert.match(forced, /FORCE/, "a force must be visibly different, not just a count");
  assert.match(forced, /2 ahead, 3 behind/);
  assert.match(forced, /origin\/feat/);

  const ff = pendingLine(outcome({ status: "would-fast-forward", branch: "feat", ahead: 4 }));
  assert.match(ff, /fast-forward \(4 ahead\)/);
  assert.doesNotMatch(ff, /FORCE/);

  assert.match(pendingLine(outcome({ status: "would-create", branch: "feat" })), /new branch/);
});

test("pendingLine omits the destination when no remote resolved", () => {
  const line = pendingLine(
    outcome({ status: "would-create", branch: "feat", remote: "", remote_branch: "" }),
  );
  assert.doesNotMatch(line, /→/);
});

test("confirmDetail lists forced branches first and explains the lease", () => {
  const detail = confirmDetail(
    [
      outcome({ status: "would-fast-forward", branch: "ff", ahead: 1 }),
      outcome({ status: "would-force", branch: "forced", ahead: 1, behind: 1 }),
    ],
    [outcome({ status: "skipped", branch: "skipped", reason: "no-remote" })],
  );
  assert.ok(
    detail.indexOf("forced") < detail.indexOf("ff"),
    `the consequential half must lead:\n${detail}`,
  );
  assert.match(detail, /Skipped:/);
  assert.match(detail, /no remote to publish to/);
  assert.match(detail, /--force-with-lease --force-if-includes/);
  assert.match(detail, /refused rather than\s+overwriting work you have not seen/);
});

test("confirmDetail omits the lease explanation when nothing is forced", () => {
  const detail = confirmDetail([outcome({ status: "would-fast-forward", ahead: 1 })], []);
  assert.doesNotMatch(detail, /force-with-lease/);
});

test("confirmTitle names the force count so a rewrite is never confirmed blind", () => {
  assert.equal(
    confirmTitle([outcome({ status: "would-force", branch: "feat" })], 1),
    "Force-push “feat” with a lease?",
  );
  assert.equal(
    confirmTitle([outcome({ status: "would-fast-forward", branch: "feat" })], 1),
    "Push “feat”?",
  );
  assert.equal(
    confirmTitle(
      [
        outcome({ status: "would-force", branch: "a" }),
        outcome({ status: "would-fast-forward", branch: "b" }),
      ],
      5,
    ),
    "Push 2 of 5 branches (1 force-pushed)?",
  );
});

test("nothingToPushMessage names why, so the action never looks like a silent failure", () => {
  assert.match(
    nothingToPushMessage({ worktrees: [outcome({ status: "up-to-date" })] }, 1),
    /all already up to date/,
  );
  assert.match(
    nothingToPushMessage(
      {
        worktrees: [
          outcome({ status: "skipped", reason: "default-branch-force-push" }),
          outcome({ status: "up-to-date" }),
        ],
      },
      2,
    ),
    /1 of 2 skipped \(refusing to force-push the default branch\), 1 already up to date/,
  );
  assert.equal(nothingToPushMessage({}, 0), "nothing to push");
});

test("skipReasonText renders each slug and falls through to an unknown one", () => {
  assert.equal(skipReasonText("detached-head"), "detached HEAD");
  assert.equal(skipReasonText("no-remote"), "no remote to publish to");
  assert.equal(
    skipReasonText("default-branch-force-push"),
    "refusing to force-push the default branch",
  );
  assert.equal(
    skipReasonText("something-a-newer-daemon-invented"),
    "something-a-newer-daemon-invented",
    "an unknown slug must render rather than vanish",
  );
});

test("outcomeLabel falls back to the folder basename without a branch", () => {
  assert.equal(outcomeLabel(outcome({ status: "pushed", branch: undefined })), "feature");
  assert.equal(
    outcomeLabel(outcome({ status: "pushed", branch: undefined, path: "/a/b/wt-c/" })),
    "wt-c",
  );
});

test("summarize reports a lease refusal as an error naming the fix", () => {
  const { severity, message } = summarize({
    worktrees: [
      outcome({ status: "pushed", branch: "a", forced: true }),
      outcome({ status: "rejected", branch: "b", detail: "stale info", stale: true }),
    ],
  });
  assert.equal(severity, "error");
  assert.match(message, /pushed 1 branch \(1 force-pushed\)/);
  assert.match(message, /refused because the remote moved: b/);
  assert.match(message, /git fetch/, "the toast must say what to do about it");
});

test("summarize keeps an ordinary rejection distinct from a lease refusal", () => {
  const { severity, message } = summarize({
    worktrees: [
      outcome({ status: "rejected", branch: "b", detail: "pre-receive hook declined" }),
    ],
  });
  assert.equal(severity, "error");
  assert.match(message, /1 rejected: b \(pre-receive hook declined\)/);
  assert.doesNotMatch(message, /remote moved/);
});

test("summarize counts created branches separately from pushed ones", () => {
  const { severity, message } = summarize({
    worktrees: [
      outcome({ status: "pushed", branch: "a", forced: false }),
      outcome({ status: "created", branch: "b" }),
    ],
  });
  assert.equal(severity, "info");
  assert.match(message, /pushed 1 branch/);
  assert.doesNotMatch(message, /force-pushed/, "a fast-forward is not a force push");
  assert.match(message, /published 1 new/);
});

test("summarize says so plainly when nothing happened", () => {
  assert.deepEqual(summarize({}), { severity: "info", message: "nothing was pushed" });
  assert.deepEqual(summarize({ worktrees: [outcome({ status: "up-to-date" })] }), {
    severity: "info",
    message: "nothing was pushed",
  });
});
