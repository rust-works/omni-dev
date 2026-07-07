// Unit tests for the pure protocol/path logic. Run with `node --test out/`
// after `tsc` emits this to `out/socket.test.js`. Nothing here imports
// `vscode`, so it runs under a plain Node process.

import assert from "node:assert/strict";
import { test } from "node:test";
import * as path from "path";
import {
  MAX_SOCKET_PATH_LEN,
  checkSocketPathLen,
  defaultDataDir,
  defaultSocketPath,
  heartbeatEnvelope,
  registerEnvelope,
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
