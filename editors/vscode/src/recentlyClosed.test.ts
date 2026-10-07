// Unit tests for the pure Recently Closed model (#2211). Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import * as fs from "fs";
import * as path from "path";
import { test } from "node:test";

import {
  CLOSED_GROUP_CONTEXT,
  UNCOMMITTED_CAVEAT,
  closedBranchLabel,
  closedContextValue,
  closedDescription,
  closedIconId,
  closedLabel,
  closedPickItems,
  closedShortName,
  closedTooltip,
  findClosed,
  formatAge,
  notRestorableMessage,
  reopenConfirmation,
  reopenedMessage,
  sameClosed,
} from "./recentlyClosed";
import { ClosedWorktreePayload, ReopenPlan } from "./socket";

const NOW = new Date("2026-10-07T12:00:00Z");

function closed(partial: Partial<ClosedWorktreePayload> = {}): ClosedWorktreePayload {
  return {
    path: "/wt/omni-dev/issue-2211",
    repo_root: "/home/me/omni-dev",
    main_repo: "omni-dev",
    branch: "issue-2211",
    head_sha: "0123456789abcdef0123456789abcdef01234567",
    is_main: false,
    removed: false,
    closed_at: "2026-10-07T11:55:00Z",
    ...partial,
  };
}

test("formatAge reads minutes, hours and days, and never goes negative", () => {
  assert.equal(formatAge("2026-10-07T11:59:30Z", NOW), "just now");
  assert.equal(formatAge("2026-10-07T11:55:00Z", NOW), "5m ago");
  assert.equal(formatAge("2026-10-07T11:00:01Z", NOW), "59m ago");
  assert.equal(formatAge("2026-10-07T09:00:00Z", NOW), "3h ago");
  assert.equal(formatAge("2026-10-05T12:00:00Z", NOW), "2d ago");
  // Clock skew between processes can put `closed_at` slightly in the future.
  assert.equal(formatAge("2026-10-07T12:00:05Z", NOW), "just now");
  // An unparseable stamp drops the age instead of printing NaN.
  assert.equal(formatAge("not a time", NOW), "");
});

test("a linked worktree is labelled repo · branch, with (detached) when there is no branch", () => {
  assert.equal(closedLabel(closed()), "omni-dev · issue-2211");
  assert.equal(closedLabel(closed({ branch: undefined })), "omni-dev · (detached)");
  assert.equal(closedBranchLabel(closed({ branch: undefined })), "(detached)");
});

test("the description is the age, plus removed when the worktree was deleted", () => {
  assert.equal(closedDescription(closed(), NOW), "5m ago");
  assert.equal(closedDescription(closed({ removed: true }), NOW), "5m ago · removed");
  assert.equal(closedDescription(closed({ is_main: true }), NOW), "5m ago · main tree");
  assert.equal(
    closedDescription(closed({ removed: true, closed_at: "garbage" }), NOW),
    "removed",
  );
});

test("removed and window-closed rows differ in icon and context value", () => {
  assert.equal(closedIconId(closed({ removed: true })), "trash");
  assert.equal(closedIconId(closed()), "history");
  assert.equal(closedContextValue(closed({ removed: true })), "closed.removed");
  assert.equal(closedContextValue(closed()), "closed.window");
});

test("the tooltip carries the full path, the close time and what reopening does", () => {
  const tip = closedTooltip(closed({ removed: true }));
  assert.ok(tip.startsWith("/wt/omni-dev/issue-2211\n"));
  assert.ok(tip.includes("Closed: 2026-10-07T11:55:00Z"));
  assert.ok(tip.includes("Commit: 0123456"));
  assert.ok(tip.includes("recreates it after you confirm"));
  assert.ok(closedTooltip(closed()).includes("Only the window was closed"));
  assert.ok(!closedTooltip(closed({ head_sha: undefined })).includes("Commit:"));
});

test("pick items keep the order received and carry the recorded path", () => {
  const items = closedPickItems(
    [
      closed({ path: "/wt/a", branch: "newer" }),
      closed({ path: "/wt/b", branch: "older", removed: true }),
    ],
    NOW,
  );
  assert.deepEqual(
    items.map((i) => [i.label, i.description, i.detail, i.path]),
    [
      ["omni-dev · newer", "5m ago", "/wt/a", "/wt/a"],
      ["omni-dev · older", "5m ago · removed", "/wt/b", "/wt/b"],
    ],
  );
  assert.deepEqual(closedPickItems([], NOW), []);
});

test("sameClosed compares by value and treats a reorder as a change", () => {
  const a = closed({ path: "/wt/a" });
  const b = closed({ path: "/wt/b" });
  assert.equal(sameClosed([a, b], [{ ...a }, { ...b }]), true);
  assert.equal(sameClosed([a, b], [b, a]), false);
  assert.equal(sameClosed([], [a]), false);
  assert.equal(sameClosed([], []), true);
});

test("findClosed and closedShortName", () => {
  const a = closed({ path: "/wt/a" });
  assert.equal(findClosed([a], "/wt/a"), a);
  assert.equal(findClosed([a], "/wt/zzz"), undefined);
  assert.equal(closedShortName(closed({ branch: undefined, path: "/wt/x/" })), "x");
  assert.equal(closedShortName(closed()), "issue-2211");
});

test("a branch restore names the branch, the destination and the uncommitted caveat", () => {
  const plan: ReopenPlan = { restorable: true, source: "branch", branch: "issue-2211", warnings: [] };
  const c = reopenConfirmation(closed({ removed: true }), plan);
  assert.equal(c.message, "Recreate worktree “omni-dev · issue-2211”?");
  assert.equal(c.confirmLabel, "Recreate Worktree");
  assert.deepEqual(c.detail.split("\n"), [
    "Branch: issue-2211",
    "Recreated at: /wt/omni-dev/issue-2211",
    "Source: the existing branch “issue-2211”",
    "",
    UNCOMMITTED_CAVEAT,
  ]);
});

test("a head-sha restore names the recorded commit and says the branch is gone", () => {
  const plan: ReopenPlan = {
    restorable: true,
    source: "head-sha",
    branch: "issue-2211",
    head_sha: "fedcba9876543210",
    warnings: ["the branch's remote was deleted"],
  };
  const lines = reopenConfirmation(closed({ removed: true }), plan).detail.split("\n");
  assert.ok(lines.includes("Source: the recorded commit fedcba9; the branch “issue-2211” no longer exists"));
  assert.ok(lines.includes("• the branch's remote was deleted"));
  // The caveat is last and unconditional.
  assert.equal(lines[lines.length - 1], UNCOMMITTED_CAVEAT);
});

test("the confirmation falls back to the recorded branch and sha, and to a detached HEAD", () => {
  const entry = closed({ removed: true });
  const fallback = reopenConfirmation(entry, { restorable: true, source: "head-sha" }).detail;
  assert.ok(fallback.includes("Branch: issue-2211"));
  assert.ok(fallback.includes("the recorded commit 0123456"));

  const detached = reopenConfirmation(closed({ removed: true, branch: undefined, head_sha: undefined }), {
    restorable: true,
    source: "head-sha",
  }).detail;
  assert.ok(detached.includes("Branch: (detached HEAD)"));
  assert.ok(detached.includes("Source: the recorded commit\n"));
  assert.ok(detached.includes(UNCOMMITTED_CAVEAT));
});

test("every confirmation carries the caveat, whatever the plan", () => {
  for (const plan of [
    { restorable: true } as ReopenPlan,
    { restorable: true, source: "branch" } as ReopenPlan,
    { restorable: true, source: "head-sha", warnings: ["w"] } as ReopenPlan,
  ]) {
    assert.ok(reopenConfirmation(closed({ removed: true }), plan).detail.includes(UNCOMMITTED_CAVEAT));
  }
});

test("a plan that cannot be restored reports the daemon's reason", () => {
  const entry = closed({ removed: true });
  assert.equal(
    notRestorableMessage(entry, { restorable: false, reason: "branch and commit are both gone" }),
    "omni-dev: cannot reopen “omni-dev · issue-2211” — branch and commit are both gone.",
  );
  assert.equal(
    notRestorableMessage(entry, { restorable: false, reason: "  " }),
    "omni-dev: cannot reopen “omni-dev · issue-2211” — it can no longer be recreated.",
  );
});

test("the success toast says recreated only when the worktree was recreated", () => {
  assert.equal(reopenedMessage(closed(), false), "omni-dev: reopened “omni-dev · issue-2211”.");
  assert.equal(
    reopenedMessage(closed({ removed: true }), true),
    "omni-dev: recreated and reopened “omni-dev · issue-2211”.",
  );
});

test("no existing menu clause matches a Recently Closed row or its group", () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"));
  const clauses: { command: string; when: string }[] = pkg.contributes.menus["view/item/context"];
  const values = [
    closedContextValue(closed()),
    closedContextValue(closed({ removed: true })),
    CLOSED_GROUP_CONTEXT,
  ];
  for (const { command, when } of clauses) {
    const m = /viewItem =~ \/(.+?)\/(?: |$)/.exec(when);
    if (!m) {
      continue;
    }
    const re = new RegExp(m[1]);
    const matches = values.filter((v) => re.test(v));
    if (command === "omniDevWorktrees.reopenClosedWorktree") {
      assert.deepEqual(matches, values.slice(0, 2), `${command}: ${when}`);
    } else {
      assert.deepEqual(matches, [], `${command} must not match a closed row: ${when}`);
    }
  }
  // The reopen command is actually contributed as a menu entry and a command.
  assert.ok(clauses.some((c) => c.command === "omniDevWorktrees.reopenClosedWorktree"));
  assert.ok(
    pkg.contributes.commands.some(
      (c: { command: string }) => c.command === "omniDevWorktrees.reopenClosedWorktree",
    ),
  );
});
