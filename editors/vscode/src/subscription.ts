// The long-lived push-subscription client for the worktrees `tree` view.
//
// Like `socket.ts` this module is `vscode`-free so it is unit-testable under
// `node --test` against a plain `net` server. It opens ONE persistent connection
// to the daemon control socket, sends a single `subscribe` line, and then only
// reads: the daemon pushes an initial `tree` snapshot followed by a fresh one on
// every real change (`src/daemon/server.rs` `run_stream`). Writing any further
// line is treated by the daemon as a cancel, so this client never writes again —
// it unsubscribes by closing the socket. A dropped/absent daemon triggers a
// silent exponential-backoff reconnect loop; nothing here ever throws at the
// caller, matching the reporter's "daemon down is a no-op" contract.

import * as net from "net";

import { Reply, checkSocketPathLen, subscribeEnvelope } from "./socket";
import { TreeSnapshot } from "./tree";

/** Injectable collaborators + backoff tuning (defaults wire real timers). */
export interface TreeSubscriptionOptions {
  /**
   * Called with every pushed snapshot: its `repos` and the daemon-backed
   * `show_closed` toggle (#1301), so the reader drives both the tree and the
   * show/hide-closed filter from the same authoritative frame.
   */
  onSnapshot: (snapshot: TreeSnapshot) => void;
  /** Called on connect↔disconnect transitions (drives the daemon-down hint). */
  onStatus?: (connected: boolean) => void;
  /** Called with a human-readable message on each recoverable drop. */
  onError?: (message: string) => void;
  /** First reconnect delay; doubles each failure up to `maxBackoffMs`. */
  initialBackoffMs?: number;
  /** Reconnect backoff ceiling. */
  maxBackoffMs?: number;
  /** Timer hooks, injected so tests drive reconnection deterministically. */
  setTimeoutFn?: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimeoutFn?: (handle: ReturnType<typeof setTimeout>) => void;
  /** Jitter source in `[0, 1)`; injected for deterministic tests. */
  random?: () => number;
}

const DEFAULT_INITIAL_BACKOFF_MS = 500;
const DEFAULT_MAX_BACKOFF_MS = 10_000;

/**
 * A resilient subscription to the daemon's worktrees `tree` stream. Construct
 * it, then call {@link start}; call {@link close} to tear it down (idempotent,
 * safe to hand to `context.subscriptions`).
 */
export class TreeSubscription {
  private readonly onSnapshot: (snapshot: TreeSnapshot) => void;
  private readonly onStatus?: (connected: boolean) => void;
  private readonly onError?: (message: string) => void;
  private readonly initialBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly setTimeoutFn: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  private readonly clearTimeoutFn: (handle: ReturnType<typeof setTimeout>) => void;
  private readonly random: () => number;

  private conn?: net.Socket;
  private buf = "";
  private backoff: number;
  private reconnectTimer?: ReturnType<typeof setTimeout>;
  private closed = false;
  private connected = false;

  constructor(
    private readonly socketPath: string,
    options: TreeSubscriptionOptions,
  ) {
    this.onSnapshot = options.onSnapshot;
    this.onStatus = options.onStatus;
    this.onError = options.onError;
    this.initialBackoffMs = options.initialBackoffMs ?? DEFAULT_INITIAL_BACKOFF_MS;
    this.maxBackoffMs = options.maxBackoffMs ?? DEFAULT_MAX_BACKOFF_MS;
    this.setTimeoutFn = options.setTimeoutFn ?? ((cb, ms) => setTimeout(cb, ms));
    this.clearTimeoutFn = options.clearTimeoutFn ?? ((handle) => clearTimeout(handle));
    this.random = options.random ?? Math.random;
    this.backoff = this.initialBackoffMs;
  }

  /** Opens the subscription and begins the reconnect loop. */
  start(): void {
    // A too-long socket path can never connect; fail permanently (logged) rather
    // than spin a doomed reconnect loop or throw into activation.
    try {
      checkSocketPathLen(this.socketPath);
    } catch (err) {
      this.closed = true;
      this.onError?.(err instanceof Error ? err.message : String(err));
      return;
    }
    this.connect();
  }

  /** Tears the subscription down: stops reconnects and drops the socket. */
  close(): void {
    this.closed = true;
    if (this.reconnectTimer !== undefined) {
      this.clearTimeoutFn(this.reconnectTimer);
      this.reconnectTimer = undefined;
    }
    if (this.conn) {
      this.conn.destroy();
      this.conn = undefined;
    }
  }

  private connect(): void {
    if (this.closed) {
      return;
    }
    const conn = net.createConnection(this.socketPath);
    this.conn = conn;
    this.buf = "";

    // `error` fires then `close`, so guard so exactly one drop is handled per
    // connection and later events on this (already-replaced) socket are ignored.
    let settled = false;
    const drop = (message: string) => {
      if (settled) {
        return;
      }
      settled = true;
      if (this.conn === conn) {
        this.conn = undefined;
      }
      conn.destroy();
      this.handleDrop(message);
    };

    conn.on("connect", () => {
      // The one and only write: request the stream. Any further write would be
      // read by the daemon as a cancel.
      conn.write(JSON.stringify(subscribeEnvelope()) + "\n");
    });
    conn.on("data", (chunk: Buffer) => this.onData(chunk));
    conn.on("error", (err: Error) => drop(err.message));
    conn.on("end", () => drop("daemon ended the stream"));
    conn.on("close", () => drop("connection closed"));
  }

  private onData(chunk: Buffer): void {
    this.buf += chunk.toString("utf8");
    let nl = this.buf.indexOf("\n");
    while (nl >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.trim().length > 0) {
        this.onLine(line);
      }
      nl = this.buf.indexOf("\n");
    }
  }

  private onLine(line: string): void {
    let reply: Reply;
    try {
      reply = JSON.parse(line) as Reply;
    } catch (err) {
      this.onError?.(`malformed snapshot: ${err instanceof Error ? err.message : String(err)}`);
      return;
    }
    // Ignore anything that is not a well-formed `tree` snapshot (e.g. an error
    // reply); a fresh snapshot is the only frame the stream should carry.
    if (reply.ok && reply.payload && Array.isArray(reply.payload.repos)) {
      // Any successful snapshot proves the daemon is up: reset backoff and, on
      // the first one, announce the connection.
      this.backoff = this.initialBackoffMs;
      if (!this.connected) {
        this.connected = true;
        this.onStatus?.(true);
      }
      this.onSnapshot(reply.payload as TreeSnapshot);
    }
  }

  private handleDrop(message: string): void {
    if (this.closed) {
      return;
    }
    if (this.connected) {
      this.connected = false;
      this.onStatus?.(false);
    }
    this.onError?.(message);
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.closed) {
      return;
    }
    // Full jitter on the high side: delay ∈ [backoff, 1.5·backoff).
    const delay = this.backoff + this.backoff * 0.5 * this.random();
    this.reconnectTimer = this.setTimeoutFn(() => {
      this.reconnectTimer = undefined;
      this.connect();
    }, delay);
    this.backoff = Math.min(this.backoff * 2, this.maxBackoffMs);
  }
}
