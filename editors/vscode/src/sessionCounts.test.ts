// Unit tests for the pure Claude-session cue model. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  Family,
  SessionEntry,
  SessionTally,
  classifyModel,
  decodeSessionTally,
  encodeSessionTally,
  formatModelMarker,
  sameModelFamilies,
  sameTallies,
  sessionDecoration,
  sessionGlyphs,
  sessionTooltipLine,
  sessionTotal,
  tallyByWorktree,
  tallyModelsByWorktree,
  unionModelFamilies,
} from "./sessionCounts";

const PATHS = ["/w/repo", "/w/repo-two", "/w/repo/nested"];

function session(
  cwd: string | undefined,
  state: SessionEntry["state"],
  model?: string,
): SessionEntry {
  return { session_id: `s-${cwd ?? "none"}-${state}`, cwd, state, model };
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

test("sessionGlyphs renders non-empty buckets, most in need of attention first", () => {
  assert.equal(sessionGlyphs({ working: 2, waiting: 1, idle: 3 }), "!1 ◦3 ⚙2");
  assert.equal(sessionGlyphs({ working: 1, waiting: 0, idle: 0 }), "⚙1");
  // Nothing to say → empty, so it drops out of the row description entirely.
  assert.equal(sessionGlyphs({ working: 0, waiting: 0, idle: 0 }), "");
  assert.equal(sessionGlyphs(undefined), "");
});

test("sessionDecoration ranks waiting over idle over working", () => {
  assert.deepEqual(sessionDecoration({ working: 3, waiting: 1, idle: 2 }), {
    badge: "!1",
    colorId: "charts.yellow",
    tooltip: "Claude: 1 waiting on you, 2 idle, 3 working",
  });
  assert.equal(sessionDecoration({ working: 3, waiting: 0, idle: 2 })?.badge, "◦2");
  assert.equal(
    sessionDecoration({ working: 3, waiting: 0, idle: 2 })?.colorId,
    "descriptionForeground",
  );
  assert.equal(sessionDecoration({ working: 3, waiting: 0, idle: 0 })?.badge, "⚙3");
  assert.equal(sessionDecoration({ working: 3, waiting: 0, idle: 0 })?.colorId, "charts.green");
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

test("sameTallies compares maps by value so an unchanged poll is a no-op", () => {
  const a = { "/w/repo": { working: 1, waiting: 0, idle: 2 } };
  assert.ok(sameTallies(a, { "/w/repo": { working: 1, waiting: 0, idle: 2 } }));
  assert.ok(!sameTallies(a, { "/w/repo": { working: 2, waiting: 0, idle: 2 } }));
  assert.ok(!sameTallies(a, {}));
  assert.ok(!sameTallies(a, { "/w/other": { working: 1, waiting: 0, idle: 2 } }));
  assert.ok(sameTallies({}, {}));
});

test("classifyModel matches each family by case-insensitive substring (#1448)", () => {
  assert.equal(classifyModel("claude-haiku-4-5-20251001"), "h");
  assert.equal(classifyModel("claude-SONNET-4-6"), "s");
  assert.equal(classifyModel("claude-opus-4-8"), "o");
  assert.equal(classifyModel("claude-fable-5"), "f");
});

test("classifyModel survives a Bedrock/regional-prefixed id", () => {
  assert.equal(classifyModel("us.anthropic.claude-3-7-sonnet-20250219-v1:0"), "s");
});

test("classifyModel falls back to * for an unrecognized or empty id", () => {
  assert.equal(classifyModel("some-other-vendor-model"), "*");
  assert.equal(classifyModel(""), "*");
});

test("tallyModelsByWorktree buckets each session's family onto its worktree", () => {
  const families = tallyModelsByWorktree(
    [
      session("/w/repo", "working", "claude-sonnet-4-6"),
      session("/w/repo", "idle", "claude-opus-4-8"), // idle still counts (#1448 scope)
    ],
    PATHS,
  );
  assert.deepEqual([...families["/w/repo"]].sort(), ["o", "s"]);
});

test("tallyModelsByWorktree yields no entry for a worktree with no sessions", () => {
  assert.deepEqual(tallyModelsByWorktree([], PATHS), {});
});

test("tallyModelsByWorktree drops what tallyByWorktree also drops", () => {
  const families = tallyModelsByWorktree(
    [
      session("/w/repo", "ended", "claude-opus-4-8"),
      session(undefined, "working", "claude-opus-4-8"),
      session("/elsewhere", "working", "claude-opus-4-8"),
    ],
    PATHS,
  );
  assert.deepEqual(families, {});
});

test("tallyModelsByWorktree classifies a session with no learned model as *", () => {
  const families = tallyModelsByWorktree([session("/w/repo", "working")], PATHS);
  assert.deepEqual([...families["/w/repo"]], ["*"]);
});

test("unionModelFamilies unions across every worktree of a repo", () => {
  const families = {
    "/w/repo": new Set<Family>(["s"]),
    "/w/repo/nested": new Set<Family>(["o"]),
  };
  assert.deepEqual(
    [...unionModelFamilies(["/w/repo", "/w/repo/nested"], families)].sort(),
    ["o", "s"],
  );
});

test("unionModelFamilies is empty for a repo with no sessions anywhere", () => {
  assert.deepEqual(unionModelFamilies(["/w/repo", "/w/repo-two"], {}), new Set());
});

test("formatModelMarker renders in fixed h/s/o/f/* order regardless of insertion order", () => {
  assert.equal(formatModelMarker(new Set<Family>(["o", "h"])), "[ho]");
  assert.equal(formatModelMarker(new Set<Family>(["*", "f", "s"])), "[sf*]");
});

test("formatModelMarker is empty (not '[]') for an empty or absent set", () => {
  assert.equal(formatModelMarker(new Set()), "");
  assert.equal(formatModelMarker(undefined), "");
});

test("sameModelFamilies compares maps by value", () => {
  const a = { "/w/repo": new Set<Family>(["s", "o"]) };
  assert.ok(sameModelFamilies(a, { "/w/repo": new Set<Family>(["o", "s"]) }));
  assert.ok(!sameModelFamilies(a, { "/w/repo": new Set<Family>(["s"]) }));
  assert.ok(!sameModelFamilies(a, {}));
});
