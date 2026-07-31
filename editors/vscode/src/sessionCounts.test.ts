// Unit tests for the pure Claude-session cue model. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  SessionEntry,
  SessionTally,
  countUnmatchedSessions,
  decodeSessionTally,
  encodeSessionTally,
  sameTallies,
  sessionDecoration,
  sessionGlyphs,
  sessionTooltipLine,
  sessionTotal,
  tallyByWorktree,
} from "./sessionCounts";

const PATHS = ["/w/repo", "/w/repo-two", "/w/repo/nested"];

function session(cwd: string | undefined, state: SessionEntry["state"]): SessionEntry {
  return { session_id: `s-${cwd ?? "none"}-${state}`, cwd, state };
}

test("tallyByWorktree buckets each state onto its worktree", () => {
  const tallies = tallyByWorktree(
    [
      session("/w/repo/src", "working"),
      session("/w/repo", "starting"),
      session("/w/repo/docs", "waiting_for_permission"),
      session("/w/repo", "waiting_for_input"),
      session("/w/repo", "idle"),
    ],
    PATHS,
  );
  assert.deepEqual(tallies["/w/repo"], { working: 2, waiting: 2, idle: 1 });
});

test("tallyByWorktree drops sessions it cannot or should not attribute", () => {
  const tallies = tallyByWorktree(
    [
      session("/w/repo", "ended"), // a tombstone, not a cue
      session(undefined, "working"), // no cwd learned yet
      session("/elsewhere", "working"), // outside every worktree
    ],
    PATHS,
  );
  assert.deepEqual(tallies, {});
});

test("tallyByWorktree matches on a path boundary, longest first", () => {
  const tallies = tallyByWorktree(
    [
      // A sibling whose path merely starts with another's must not be claimed.
      session("/w/repo-two/src", "working"),
      // A nested worktree wins over its containing one.
      session("/w/repo/nested/src", "working"),
      session("/w/repo/src", "idle"),
    ],
    PATHS,
  );
  assert.deepEqual(tallies["/w/repo-two"], { working: 1, waiting: 0, idle: 0 });
  assert.deepEqual(tallies["/w/repo/nested"], { working: 1, waiting: 0, idle: 0 });
  assert.deepEqual(tallies["/w/repo"], { working: 0, waiting: 0, idle: 1 });
});

test("tallyByWorktree attributes a session sitting exactly at the worktree root", () => {
  const tallies = tallyByWorktree([session("/w/repo", "working")], ["/w/repo/"]);
  assert.deepEqual(tallies["/w/repo/"], { working: 1, waiting: 0, idle: 0 });
});

test("sessionGlyphs renders non-empty buckets, most urgent first", () => {
  assert.equal(sessionGlyphs({ working: 2, waiting: 1, idle: 3 }), "!1 ⚙2 ◦3");
  assert.equal(sessionGlyphs({ working: 1, waiting: 0, idle: 0 }), "⚙1");
  // Nothing to say → empty, so it drops out of the row description entirely.
  assert.equal(sessionGlyphs({ working: 0, waiting: 0, idle: 0 }), "");
  assert.equal(sessionGlyphs(undefined), "");
});

test("sessionDecoration ranks waiting over working over idle", () => {
  assert.deepEqual(sessionDecoration({ working: 3, waiting: 1, idle: 2 }), {
    badge: "!1",
    colorId: "charts.yellow",
    tooltip: "Claude: 1 waiting on you, 3 working, 2 idle",
  });
  assert.equal(sessionDecoration({ working: 3, waiting: 0, idle: 2 })?.badge, "⚙3");
  assert.equal(sessionDecoration({ working: 3, waiting: 0, idle: 2 })?.colorId, "charts.green");
  assert.equal(sessionDecoration({ working: 0, waiting: 0, idle: 2 })?.badge, "◦2");
  assert.equal(
    sessionDecoration({ working: 0, waiting: 0, idle: 2 })?.colorId,
    "descriptionForeground",
  );
  assert.equal(sessionDecoration({ working: 0, waiting: 0, idle: 0 }), undefined);
  assert.equal(sessionDecoration(undefined), undefined);
});

test("sessionDecoration keeps the badge within the two-character limit", () => {
  assert.equal(sessionDecoration({ working: 9, waiting: 0, idle: 0 })?.badge, "⚙9");
  assert.equal(sessionDecoration({ working: 10, waiting: 0, idle: 0 })?.badge, "⚙+");
  assert.equal(sessionDecoration({ working: 0, waiting: 42, idle: 0 })?.badge, "!+");
});

test("sessionTooltipLine names each bucket, or nothing at all", () => {
  assert.equal(
    sessionTooltipLine({ working: 1, waiting: 2, idle: 0 }),
    "Claude: 2 waiting on you, 1 working",
  );
  assert.equal(sessionTooltipLine({ working: 0, waiting: 0, idle: 0 }), undefined);
  assert.equal(sessionTooltipLine(undefined), undefined);
});

test("a tally round-trips through the resourceUri query", () => {
  const tally: SessionTally = { working: 2, waiting: 1, idle: 0 };
  assert.equal(encodeSessionTally(tally), "2-1-0");
  assert.deepEqual(decodeSessionTally(encodeSessionTally(tally)), tally);
  assert.equal(sessionTotal(tally), 3);
});

test("decodeSessionTally rejects anything it did not write", () => {
  for (const bad of [null, undefined, "", "1-2", "1-2-3-4", "a-b-c", "1--1-2", "1.5-0-0"]) {
    assert.equal(decodeSessionTally(bad), undefined, `expected ${String(bad)} to be rejected`);
  }
});

test("countUnmatchedSessions counts a real cwd outside every worktree, not a cwd not yet known", () => {
  const sessions = [
    session("/w/repo", "ended"), // a tombstone, not unattributed — excluded
    session(undefined, "working"), // no cwd learned yet — a transient state, not counted
    session("/elsewhere", "working"), // a real cwd outside every worktree — counted
    session("/w/repo", "idle"), // attributed fine — not counted
  ];
  assert.equal(countUnmatchedSessions(sessions, PATHS), 1);
  assert.deepEqual(tallyByWorktree(sessions, PATHS), { "/w/repo": { working: 0, waiting: 0, idle: 1 } });
});

test("countUnmatchedSessions is zero when every live session matches a worktree", () => {
  assert.equal(countUnmatchedSessions([session("/w/repo", "working")], PATHS), 0);
  assert.equal(countUnmatchedSessions([], PATHS), 0);
});

test("sameTallies compares maps by value so an unchanged poll is a no-op", () => {
  const a = { "/w/repo": { working: 1, waiting: 0, idle: 2 } };
  assert.ok(sameTallies(a, { "/w/repo": { working: 1, waiting: 0, idle: 2 } }));
  assert.ok(!sameTallies(a, { "/w/repo": { working: 2, waiting: 0, idle: 2 } }));
  assert.ok(!sameTallies(a, {}));
  assert.ok(!sameTallies(a, { "/w/other": { working: 1, waiting: 0, idle: 2 } }));
  assert.ok(sameTallies({}, {}));
});
