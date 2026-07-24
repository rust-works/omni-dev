// Unit tests for the pure protocol/path logic. Run with `node --test out/`
// after `tsc` emits this to `out/socket.test.js`. Nothing here imports
// `vscode`, so it runs under a plain Node process.

import assert from "node:assert/strict";
import { test } from "node:test";
import * as path from "path";
import {
  MAX_SOCKET_PATH_LEN,
  aheadBehindEnvelope,
  openPrsEnvelope,
  checkSocketPathLen,
  closeCheckEnvelope,
  closeEnvelope,
  defaultDataDir,
  defaultSocketPath,
  heartbeatEnvelope,
  mergeQueueCheckEnvelope,
  mergeQueueEnvelope,
  openEnvelope,
  registerEnvelope,
  setPollingEnvelope,
  setShowClosedEnvelope,
  sessionWindowEnvelope,
  sessionWindowUnregisterEnvelope,
  subscribeEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";

const HOME = "/home/me";

test("defaultSocketPath on macOS uses Application Support and ignores XDG", () => {
  const p = defaultSocketPath({ XDG_DATA_HOME: "/should/be/ignored" }, "darwin", HOME);
  assert.equal(
    p,
    path.join(HOME, "Library", "Application Support", "omni-dev", "daemon.sock"),
  );
});

test("defaultSocketPath on linux honors an absolute XDG_DATA_HOME", () => {
  const p = defaultSocketPath({ XDG_DATA_HOME: "/xdg/data" }, "linux", HOME);
  assert.equal(p, path.join("/xdg/data", "omni-dev", "daemon.sock"));
});

test("defaultSocketPath on linux falls back to ~/.local/share", () => {
  const fallback = path.join(HOME, ".local", "share", "omni-dev", "daemon.sock");
  assert.equal(defaultSocketPath({}, "linux", HOME), fallback);
  // A relative or empty XDG_DATA_HOME is ignored, matching the dirs crate.
  assert.equal(defaultSocketPath({ XDG_DATA_HOME: "relative/path" }, "linux", HOME), fallback);
  assert.equal(defaultSocketPath({ XDG_DATA_HOME: "" }, "linux", HOME), fallback);
});

test("defaultDataDir linux with empty XDG falls back to ~/.local/share", () => {
  assert.equal(
    defaultDataDir({ XDG_DATA_HOME: "" }, "linux", HOME),
    path.join(HOME, ".local", "share"),
  );
});

test("checkSocketPathLen accepts short paths and rejects at/over the limit", () => {
  checkSocketPathLen("/tmp/short.sock"); // ok — no throw
  const atLimit = "/" + "a".repeat(MAX_SOCKET_PATH_LEN - 1);
  assert.equal(atLimit.length, MAX_SOCKET_PATH_LEN);
  assert.throws(() => checkSocketPathLen(atLimit), /exceeding the 104-byte limit/);
});

test("envelope builders match the worktrees wire contract", () => {
  assert.deepEqual(
    registerEnvelope({ key: "k1", folders: ["/a"], repo: "a", title: "a — main", pid: 42 }),
    {
      service: "worktrees",
      op: "register",
      payload: { key: "k1", folders: ["/a"], repo: "a", title: "a — main", pid: 42 },
    },
  );
  assert.deepEqual(heartbeatEnvelope("k1"), {
    service: "worktrees",
    op: "heartbeat",
    payload: { key: "k1" },
  });
  assert.deepEqual(unregisterEnvelope("k1"), {
    service: "worktrees",
    op: "unregister",
    payload: { key: "k1" },
  });
});

test("tree/subscribe/open envelope builders match the worktrees wire contract", () => {
  assert.deepEqual(treeEnvelope(), { service: "worktrees", op: "tree" });
  assert.deepEqual(subscribeEnvelope(), { service: "worktrees", op: "subscribe" });
  assert.deepEqual(openEnvelope("/home/me/wt/issue-1300"), {
    service: "worktrees",
    op: "open",
    payload: { path: "/home/me/wt/issue-1300" },
  });
});

test("ahead-behind envelope batches worktree paths for the lazy divergence op", () => {
  assert.deepEqual(aheadBehindEnvelope(["/home/me/omni-dev", "/home/me/wt/issue-1300"]), {
    service: "worktrees",
    op: "ahead-behind",
    payload: { paths: ["/home/me/omni-dev", "/home/me/wt/issue-1300"] },
  });
  // An empty batch is still well-formed (the caller skips the fetch, but the
  // builder never assumes non-empty).
  assert.deepEqual(aheadBehindEnvelope([]), {
    service: "worktrees",
    op: "ahead-behind",
    payload: { paths: [] },
  });
});

test("open-prs envelope carries the repo owner/name for the daemon-served PR list", () => {
  assert.deepEqual(openPrsEnvelope("rust-works", "omni-dev"), {
    service: "worktrees",
    op: "open-prs",
    payload: { owner: "rust-works", name: "omni-dev" },
  });
});

test("set-show-closed envelope carries the toggle as snake_case `show_closed`", () => {
  assert.deepEqual(setShowClosedEnvelope(false), {
    service: "worktrees",
    op: "set-show-closed",
    payload: { show_closed: false },
  });
  assert.deepEqual(setShowClosedEnvelope(true), {
    service: "worktrees",
    op: "set-show-closed",
    payload: { show_closed: true },
  });
});

test("set-polling envelope carries owner/name/enabled for one repo", () => {
  const repo = { owner: "rust-works", name: "omni-dev" };
  assert.deepEqual(setPollingEnvelope(repo, true), {
    service: "worktrees",
    op: "set-polling",
    payload: { owner: "rust-works", name: "omni-dev", enabled: true },
  });
  assert.deepEqual(setPollingEnvelope(repo, false), {
    service: "worktrees",
    op: "set-polling",
    payload: { owner: "rust-works", name: "omni-dev", enabled: false },
  });
});

test("sessions window envelope builders route to the sessions service", () => {
  assert.deepEqual(
    sessionWindowEnvelope({ key: "k1", folders: ["/a", "/b"], tabs: 2, terminals: 1 }),
    {
      service: "sessions",
      op: "window",
      payload: { key: "k1", folders: ["/a", "/b"], tabs: 2, terminals: 1 },
    },
  );
  assert.deepEqual(sessionWindowUnregisterEnvelope("k1"), {
    service: "sessions",
    op: "window-unregister",
    payload: { key: "k1" },
  });
});

test("close envelope builders match the two-phase worktrees wire contract", () => {
  // Phase 1: a safety check is `remove:true` with no `confirmed`.
  assert.deepEqual(closeCheckEnvelope("/wt/issue-1300", "k1"), {
    service: "worktrees",
    op: "close",
    payload: { path: "/wt/issue-1300", remove: true, requester_key: "k1" },
  });
  // Phase 2 (delete a linked worktree): `remove:true`, `confirmed:true`.
  assert.deepEqual(
    closeEnvelope("/wt/issue-1300", { remove: true, requesterKey: "k1", confirmed: true }),
    {
      service: "worktrees",
      op: "close",
      payload: { path: "/wt/issue-1300", remove: true, requester_key: "k1", confirmed: true },
    },
  );
  // "Close Window" (main tree): `remove:false`; `confirmed` is omitted when unset.
  assert.deepEqual(closeEnvelope("/repo", { remove: false, requesterKey: "k1" }), {
    service: "worktrees",
    op: "close",
    payload: { path: "/repo", remove: false, requester_key: "k1" },
  });
});

test("merge-queue envelope builders match the two-phase batched wire contract", () => {
  // Phase 1: eligibility only — `check:true`, never `confirmed`. Unlike `close`,
  // one envelope carries the whole selection as `paths`.
  assert.deepEqual(mergeQueueCheckEnvelope(["/wt/a", "/wt/b"], "k1"), {
    service: "worktrees",
    op: "merge-queue",
    payload: { paths: ["/wt/a", "/wt/b"], requester_key: "k1", check: true },
  });
  // Phase 2: execute — `confirmed:true`, never `check` (which would report only).
  assert.deepEqual(mergeQueueEnvelope(["/wt/a", "/wt/b"], "k1"), {
    service: "worktrees",
    op: "merge-queue",
    payload: { paths: ["/wt/a", "/wt/b"], requester_key: "k1", confirmed: true },
  });
  // A single-target selection is still a batch of one.
  assert.deepEqual(mergeQueueEnvelope(["/wt/a"], "k1"), {
    service: "worktrees",
    op: "merge-queue",
    payload: { paths: ["/wt/a"], requester_key: "k1", confirmed: true },
  });
});
