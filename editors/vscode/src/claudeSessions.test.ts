// Unit tests for the pure Claude-session relocation helpers (#1295). Nothing here
// imports `vscode`, so it runs under a plain Node process (`node --test out/`),
// like `tree.test.ts` / `socket.test.ts`.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  LIVE_THRESHOLD_MS,
  RelocationMode,
  encodeProjectPath,
  isLikelyLive,
  parseSessionPreview,
  planRelocation,
  relativeTime,
} from "./claudeSessions";

test("encodeProjectPath replaces every / and . with -", () => {
  assert.equal(encodeProjectPath("/Users/x/wrk/omni-dev"), "-Users-x-wrk-omni-dev");
  // A dotted leaf (verified on disk: Dot.dot → Dot-dot).
  assert.equal(
    encodeProjectPath("/Users/x/Downloads/Dot.dot"),
    "-Users-x-Downloads-Dot-dot",
  );
  // A hidden dir yields a double dash (/.work → --work).
  assert.equal(encodeProjectPath("/a/.work/issue-1"), "-a--work-issue-1");
  // Existing hyphens survive unchanged.
  assert.equal(
    encodeProjectPath("/Users/j/wrk/work-trees/omni-dev/issue-1295-x"),
    "-Users-j-wrk-work-trees-omni-dev-issue-1295-x",
  );
});

test("planRelocation always moves the transcript, sidecar only when present", () => {
  const srcDir = "/p/src";
  const destDir = "/p/dest";
  const id = "abc-123";

  const noSidecar = planRelocation({ sessionId: id, srcDir, destDir, hasSidecar: false, mode: "move" });
  assert.equal(noSidecar.sessionId, id);
  assert.equal(noSidecar.mode, "move");
  assert.deepEqual(noSidecar.ops, [
    { from: "/p/src/abc-123.jsonl", to: "/p/dest/abc-123.jsonl", kind: "file" },
  ]);

  const withSidecar = planRelocation({ sessionId: id, srcDir, destDir, hasSidecar: true, mode: "copy" });
  assert.equal(withSidecar.mode, "copy");
  assert.equal(withSidecar.ops.length, 2);
  // Transcript first, so a partial failure still yields a resumable transcript.
  assert.deepEqual(withSidecar.ops[0], {
    from: "/p/src/abc-123.jsonl",
    to: "/p/dest/abc-123.jsonl",
    kind: "file",
  });
  assert.deepEqual(withSidecar.ops[1], {
    from: "/p/src/abc-123",
    to: "/p/dest/abc-123",
    kind: "dir",
  });

  // The mode is carried through verbatim for both variants.
  const modes: RelocationMode[] = ["move", "copy"];
  for (const mode of modes) {
    assert.equal(planRelocation({ sessionId: id, srcDir, destDir, hasSidecar: false, mode }).mode, mode);
  }
});

test("parseSessionPreview prefers the first real user message text", () => {
  const lines = [
    JSON.stringify({ type: "queue-operation", sessionId: "s" }),
    JSON.stringify({ type: "user", message: { content: "Fix the flaky test in CI" } }),
  ];
  assert.equal(parseSessionPreview(lines, "s"), "Fix the flaky test in CI");
});

test("parseSessionPreview reads text from a content-array user message", () => {
  const lines = [
    JSON.stringify({
      type: "user",
      message: { content: [{ type: "text", text: "Refactor the parser" }] },
    }),
  ];
  assert.equal(parseSessionPreview(lines, "s"), "Refactor the parser");
});

test("parseSessionPreview falls back to a summary line, then the id", () => {
  // Only a summary present → the summary.
  const summaryOnly = [
    JSON.stringify({ type: "summary", summary: "Investigated the socket race" }),
    JSON.stringify({ type: "queue-operation", sessionId: "s" }),
  ];
  assert.equal(parseSessionPreview(summaryOnly, "id-1"), "Investigated the socket race");

  // Nothing usable (bookkeeping + non-JSON) → the session id.
  const nothing = [JSON.stringify({ type: "attachment" }), "not json at all"];
  assert.equal(parseSessionPreview(nothing, "id-2"), "id-2");

  // A real user message wins over an earlier summary.
  const both = [
    JSON.stringify({ type: "summary", summary: "old summary" }),
    JSON.stringify({ type: "user", message: { content: "the real prompt" } }),
  ];
  assert.equal(parseSessionPreview(both, "s"), "the real prompt");
});

test("parseSessionPreview skips bookkeeping, meta, tool-result, and slash-command lines", () => {
  const lines = [
    JSON.stringify({ type: "attachment", sessionId: "s" }),
    JSON.stringify({ type: "user", isMeta: true, message: { content: "meta noise" } }),
    JSON.stringify({ type: "user", message: { content: "<command-name>/loop</command-name>" } }),
    JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", content: "x" }] } }),
    JSON.stringify({ type: "user", message: { content: "The actual first prompt" } }),
  ];
  assert.equal(parseSessionPreview(lines, "s"), "The actual first prompt");
});

test("parseSessionPreview collapses whitespace and truncates long text", () => {
  const long = "word ".repeat(60).trim();
  const out = parseSessionPreview(
    [JSON.stringify({ type: "user", message: { content: `line1\n\n  line2   ${long}` } })],
    "s",
  );
  assert.ok(out.length <= 80, `expected <= 80 chars, got ${out.length}`);
  assert.ok(!out.includes("\n"), "expected a single line");
  assert.ok(out.endsWith("…"), "expected an ellipsis on truncation");
});

test("isLikelyLive is true within the threshold and false at or beyond it", () => {
  const now = 1_000_000;
  assert.equal(isLikelyLive(now - 1_000, now), true);
  assert.equal(isLikelyLive(now - (LIVE_THRESHOLD_MS - 1), now), true);
  assert.equal(isLikelyLive(now - LIVE_THRESHOLD_MS, now), false);
  assert.equal(isLikelyLive(now - 60_000, now), false);
  // A clock skew that puts mtime in the future is still "live".
  assert.equal(isLikelyLive(now + 5_000, now), true);
});

test("relativeTime formats compact ago-strings across units", () => {
  const now = 30 * 24 * 3600 * 1000;
  assert.equal(relativeTime(now, now), "0s ago");
  assert.equal(relativeTime(now - 5_000, now), "5s ago");
  assert.equal(relativeTime(now - 3 * 60_000, now), "3m ago");
  assert.equal(relativeTime(now - 2 * 3600_000, now), "2h ago");
  assert.equal(relativeTime(now - 5 * 24 * 3600_000, now), "5d ago");
});
