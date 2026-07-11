// Unit tests for the long-lived subscribe client. Nothing here imports `vscode`;
// the tests drive a real `net` server over a short temp unix socket, so they run
// under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import * as fs from "fs";
import * as net from "net";
import * as os from "os";
import * as path from "path";

import { TreeSubscription, ViewStateSubscription } from "./subscription";
import { TreeRepoPayload } from "./tree";
import { ViewState } from "./socket";

/** A short unix-socket path under the OS temp dir (well under the 104-byte cap). */
function tempSocketPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "odw-"));
  return path.join(dir, "d.sock");
}

/** One pushed `tree` snapshot line, matching the daemon's `DaemonReply::ok`. */
function snapshotLine(repos: TreeRepoPayload[]): string {
  return JSON.stringify({ ok: true, payload: { repos } }) + "\n";
}

/** One pushed view-state frame (`null` payload = the daemon is unseeded). */
function viewStateLine(viewState: ViewState | null): string {
  return JSON.stringify({ ok: true, payload: { view_state: viewState } }) + "\n";
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
      conn.write(snapshotLine([{ main_repo: "a", root: "/a", worktrees: [] }]));
      conn.write(snapshotLine([{ main_repo: "b", root: "/b", worktrees: [] }]));
    });
  });
  await srv.listen(socketPath);

  const received: TreeRepoPayload[][] = [];
  const statuses: boolean[] = [];
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: (repos) => received.push(repos),
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
  assert.equal(received[0][0].main_repo, "a");
  assert.equal(received[1][0].main_repo, "b");
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

  const received: TreeRepoPayload[][] = [];
  const statuses: boolean[] = [];
  const sub = new TreeSubscription(socketPath, {
    onSnapshot: (repos) => received.push(repos),
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

test("view-state: sends its subscribe line and delivers seeded and unseeded frames", async (t: TestContext) => {
  const socketPath = tempSocketPath();
  let requestLine = "";
  const srv = trackingServer((conn) => {
    conn.on("data", (chunk: Buffer) => {
      requestLine += chunk.toString("utf8");
      // Unseeded first (daemon fresh), then a seeded value.
      conn.write(viewStateLine(null));
      conn.write(viewStateLine({ show_closed: false }));
    });
  });
  await srv.listen(socketPath);

  const received: (ViewState | null)[] = [];
  const statuses: boolean[] = [];
  const sub = new ViewStateSubscription(socketPath, {
    onSnapshot: (vs) => received.push(vs),
    onStatus: (c) => statuses.push(c),
  });
  t.after(() => {
    sub.close();
    srv.close();
  });

  sub.start();
  await waitFor(() => received.length >= 2);

  assert.match(requestLine, /"op":"subscribe-view-state"/);
  assert.match(requestLine, /"service":"worktrees"/);
  // The unseeded frame arrives as `null`; the seeded one as the parsed scalar.
  assert.deepEqual(received[0], null);
  assert.deepEqual(received[1], { show_closed: false });
  // Both frames count as proof the daemon is up, announced exactly once.
  assert.deepEqual(statuses, [true]);
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
