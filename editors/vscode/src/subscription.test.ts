// Unit tests for the long-lived subscribe client. Nothing here imports `vscode`;
// the tests drive a real `net` server over a short temp unix socket, so they run
// under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import * as fs from "fs";
import * as net from "net";
import * as os from "os";
import * as path from "path";

import { SessionEntry } from "./sessionCounts";
import { SessionsSnapshot, SessionsSubscription, TreeSubscription } from "./subscription";
import { TreeRepoPayload, TreeSnapshot } from "./tree";

/** A short unix-socket path under the OS temp dir (well under the 104-byte cap). */
function tempSocketPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "odw-"));
  return path.join(dir, "d.sock");
}

/**
 * One pushed `tree` snapshot line, matching the daemon's `DaemonReply::ok`. The
 * daemon always carries the `show_closed` toggle (#1301); `showClosed` sets it
 * (defaults to `true`, the show-all default).
 */
function snapshotLine(repos: TreeRepoPayload[], showClosed = true): string {
  return JSON.stringify({ ok: true, payload: { repos, show_closed: showClosed } }) + "\n";
}

/** Polls `pred` until true, or rejects after `timeoutMs`. */
async function waitFor(pred: () => boolean, timeoutMs = 2000): Promise<void> {
  const start = Date.now();
  while (!pred()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error("waitFor timed out");
    }
    await new Promise((r) => setTimeout(r, 5));
  }
}

/** A `net` server that tracks its accepted sockets so a test can tear them down. */
function trackingServer(onConn: (conn: net.Socket, index: number) => void): {
  conns: net.Socket[];
  listen: (socketPath: string) => Promise<void>;
  close: () => void;
} {
  const conns: net.Socket[] = [];
  const server = net.createServer((conn) => {
    conns.push(conn);
    onConn(conn, conns.length);
  });
  return {
    conns,
    listen: (socketPath) => new Promise<void>((res) => server.listen(socketPath, res)),
    close: () => {
      for (const c of conns) {
        c.destroy();
      }
      server.close();
    },
  };
}

test("subscribe: sends a subscribe line and delivers pushed snapshots", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  let requestLine = "";
  const srv = trackingServer((conn) => {
    conn.on("data", (chunk: Buffer) => {
      requestLine += chunk.toString("utf8");
      conn.write(snapshotLine([{ main_repo: "a", root: "/a", worktrees: [] }], true));
      conn.write(snapshotLine([{ main_repo: "b", root: "/b", worktrees: [] }], false));
    });
  });
  await srv.listen(socketPath);

  const received: TreeSnapshot[] = [];
  const statuses: boolean[] = [];
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onStatus: (c) => statuses.push(c),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 2);

  assert.match(requestLine, /"op":"subscribe"/);
  assert.match(requestLine, /"service":"worktrees"/);
  assert.equal(received[0].repos[0].main_repo, "a");
  assert.equal(received[1].repos[0].main_repo, "b");
  // The daemon-backed toggle rides each snapshot, so the reader can drive the
  // show/hide-closed filter from the same frame (#1301).
  assert.equal(received[0].show_closed, true);
  assert.equal(received[1].show_closed, false);
  // The first successful snapshot announces the connection exactly once.
  assert.deepEqual(statuses, [true]);
});

test("subscribe: reconnects after the daemon drops the connection", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  const srv = trackingServer((conn, index) => {
    conn.on("data", () => {
      conn.write(snapshotLine([{ main_repo: `c${index}`, root: `/c${index}`, worktrees: [] }]));
      // Drop the first connection to force the client to reconnect.
      if (index === 1) {
        setTimeout(() => conn.destroy(), 10);
      }
    });
  });
  await srv.listen(socketPath);

  const received: TreeSnapshot[] = [];
  const statuses: boolean[] = [];
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onStatus: (c) => statuses.push(c),
    initialBackoffMs: 1,
    maxBackoffMs: 1,
    setTimeoutFn: (cb) => setTimeout(cb, 0), // near-instant reconnect
    random: () => 0,
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => srv.conns.length >= 2 && received.length >= 2);

  assert.ok(srv.conns.length >= 2, "should have reconnected");
  // connect → drop → reconnect: the status transitions are true, false, true.
  assert.deepEqual(statuses.slice(0, 3), [true, false, true]);
});

test("subscribe: a missing daemon retries silently, and close() stops it", async (t: TestContext) => {
  const socketPath = tempSocketPath(); // nothing is listening here
  const errors: string[] = [];
  let scheduled = 0;
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: () => {
      throw new Error("no snapshot should arrive from an absent daemon");
    },
    onError: (m) => errors.push(m),
    initialBackoffMs: 1,
    // Capture the reconnect timer without firing it, so the loop cannot spin.
    setTimeoutFn: () => {
      scheduled += 1;
      return setTimeout(() => {}, 60_000);
    },
    random: () => 0,
  });
  t.after(() => sub.close());

  sub.start();
  await waitFor(() => errors.length >= 1);
  assert.ok(scheduled >= 1, "a failed connect should schedule a reconnect");

  sub.close();
  const before = scheduled;
  await new Promise((r) => setTimeout(r, 20));
  assert.equal(scheduled, before, "no reconnect should be scheduled after close()");
});

test("subscribe: a too-long socket path fails permanently without throwing", (t: TestContext) => {
  const tooLong = "/" + "a".repeat(120);
  const errors: string[] = [];
  let scheduled = 0;
  const sub = new TreeSubscription(tooLong, {
    onSnapshot: () => {},
    onError: (m) => errors.push(m),
    setTimeoutFn: () => {
      scheduled += 1;
      return setTimeout(() => {}, 60_000);
    },
  });
  t.after(() => sub.close());

  sub.start(); // must not throw
  assert.equal(scheduled, 0, "a doomed path should not schedule reconnects");
  assert.match(errors[0], /104-byte limit/);
});

test("subscribe: an ok:true frame with the wrong shape is dropped and reported, not fatal", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  const srv = trackingServer((conn) => {
    conn.on("data", () => {
      // First frame: `ok: true` but `repos` is not an array — fails `isSnapshot`.
      conn.write(JSON.stringify({ ok: true, payload: { repos: "not-an-array" } }) + "\n");
      // Second frame: well-formed, so the stream must still deliver it.
      conn.write(snapshotLine([{ main_repo: "a", root: "/a", worktrees: [] }]));
    });
  });
  await srv.listen(socketPath);

  const received: TreeSnapshot[] = [];
  const errors: string[] = [];
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onError: (m) => errors.push(m),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 1);

  assert.equal(errors.length, 1, "the malformed frame should be reported exactly once");
  assert.match(errors[0], /dropped worktrees subscribe frame/);
  assert.equal(received[0].repos[0].main_repo, "a");
});

// --- The sessions stream (#1414) ---------------------------------------------

/** One pushed `sessions` snapshot line, matching the daemon's `DaemonReply::ok`. */
function sessionsLine(sessions: SessionEntry[]): string {
  return JSON.stringify({ ok: true, payload: { sessions } }) + "\n";
}

test("sessions subscribe: sends a sessions subscribe line and delivers snapshots", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  let requestLine = "";
  const srv = trackingServer((conn) => {
    conn.on("data", (chunk: Buffer) => {
      requestLine += chunk.toString("utf8");
      conn.write(sessionsLine([{ session_id: "s1", cwd: "/w/a", state: "working" }]));
      conn.write(sessionsLine([{ session_id: "s1", cwd: "/w/a", state: "idle" }]));
    });
  });
  await srv.listen(socketPath);

  const received: SessionsSnapshot[] = [];
  const statuses: boolean[] = [];
  const sub = new SessionsSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onStatus: (c) => statuses.push(c),
    onUnsupported: () => {
      throw new Error("a well-formed stream must not report unsupported");
    },
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 2);

  assert.match(requestLine, /"op":"subscribe"/);
  assert.match(requestLine, /"service":"sessions"/);
  // The state transition the tree renders arrives as its own pushed frame — the
  // whole point of the stream over the per-window poll.
  assert.equal(received[0].sessions[0].state, "working");
  assert.equal(received[1].sessions[0].state, "idle");
  assert.deepEqual(statuses, [true]);
});

test("sessions subscribe: an error reply reports unsupported and stops reconnecting", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  // A daemon too old to know the op: it answers, refuses, and holds the
  // connection open — so without the unsupported path the client waits forever.
  const srv = trackingServer((conn) => {
    conn.on("data", () => {
      conn.write(JSON.stringify({ ok: false, error: "unknown sessions op: subscribe" }) + "\n");
    });
  });
  await srv.listen(socketPath);

  const unsupported: string[] = [];
  let scheduled = 0;
  const sub = new SessionsSubscription(socketPath, {
    onSnapshot: () => {
      throw new Error("no snapshot should arrive from a daemon that refused");
    },
    onUnsupported: (m) => unsupported.push(m),
    initialBackoffMs: 1,
    setTimeoutFn: () => {
      scheduled += 1;
      return setTimeout(() => {}, 60_000);
    },
    random: () => 0,
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => unsupported.length >= 1);

  assert.match(unsupported[0], /unknown sessions op/);
  // Reconnecting would only re-earn the same refusal; the caller polls instead.
  await new Promise((r) => setTimeout(r, 20));
  assert.equal(scheduled, 0, "a refused op must not schedule a reconnect");
  assert.equal(unsupported.length, 1, "the refusal should be reported exactly once");
});

test("sessions subscribe: start() after a refusal revives the subscription", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  // Refuse the first connection (the "old daemon"), serve the second — the
  // daemon-restarted-after-an-upgrade path the tree's reconnect drives.
  const srv = trackingServer((conn, index) => {
    conn.on("data", () => {
      if (index === 1) {
        conn.write(JSON.stringify({ ok: false, error: "unknown sessions op: subscribe" }) + "\n");
      } else {
        conn.write(sessionsLine([{ session_id: "s1", state: "waiting_for_permission" }]));
      }
    });
  });
  await srv.listen(socketPath);

  const received: SessionsSnapshot[] = [];
  const unsupported: string[] = [];
  const sub = new SessionsSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onUnsupported: (m) => unsupported.push(m),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => unsupported.length >= 1);
  // The caller re-`start()`s on the tree subscription's reconnect.
  sub.start();
  await waitFor(() => received.length >= 1);

  assert.equal(received[0].sessions[0].state, "waiting_for_permission");
});

test("sessions subscribe: without onUnsupported an error reply is ignored, as before", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  const srv = trackingServer((conn) => {
    conn.on("data", () => {
      conn.write(JSON.stringify({ ok: false, error: "boom" }) + "\n");
      conn.write(sessionsLine([{ session_id: "s1", state: "idle" }]));
    });
  });
  await srv.listen(socketPath);

  const received: SessionsSnapshot[] = [];
  const sub = new SessionsSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 1);

  // The stream survives the error frame and keeps delivering — the pre-#1414
  // behaviour `TreeSubscription` still relies on.
  assert.equal(received[0].sessions[0].session_id, "s1");
});

test("sessions subscribe: an ok:true frame with no payload is dropped and reported", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  const srv = trackingServer((conn) => {
    conn.on("data", () => {
      // `ok: true` with no `payload` at all — also fails `isSnapshot`.
      conn.write(JSON.stringify({ ok: true }) + "\n");
      conn.write(sessionsLine([{ session_id: "s1", state: "idle" }]));
    });
  });
  await srv.listen(socketPath);

  const received: SessionsSnapshot[] = [];
  const errors: string[] = [];
  const sub = new SessionsSubscription(socketPath, {
    onSnapshot: (snapshot) => received.push(snapshot),
    onError: (m) => errors.push(m),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 1);

  assert.equal(errors.length, 1, "the malformed frame should be reported exactly once");
  assert.match(errors[0], /dropped sessions subscribe frame/);
  assert.equal(received[0].sessions[0].session_id, "s1");
});
