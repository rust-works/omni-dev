// The long-lived push-subscription clients for the daemon's streaming ops.
//
// Like `socket.ts` this module is `vscode`-free so it is unit-testable under
// `node --test` against a plain `net` server. A subscription opens ONE persistent
// connection to the daemon control socket, sends a single `subscribe` line, and
// then only reads: the daemon pushes an initial snapshot followed by a fresh one
// on every real change (`src/daemon/server.rs` `run_stream`). Writing any further
// line is treated by the daemon as a cancel, so these clients never write again —
// they unsubscribe by closing the socket. A dropped/absent daemon triggers a
// silent exponential-backoff reconnect loop; nothing here ever throws at the
// caller, matching the reporter's "daemon down is a no-op" contract.
//
// `DaemonSubscription` holds all of that machinery; `TreeSubscription` (worktrees
// `tree`, #1267) and `SessionsSubscription` (sessions state, #1414) are thin
// bindings of an envelope to a payload guard.

import * as net from "net";

import {
  Envelope,
  Reply,
  checkSocketPathLen,
  sessionsSubscribeEnvelope,
  subscribeEnvelope,
} from "./socket";
import { SessionEntry } from "./sessionCounts";
import { TreeSnapshot } from "./tree";

/** Injectable collaborators + backoff tuning (defaults wire real timers). */
export interface SubscriptionOptions<T> {
  /** Called with every pushed snapshot that passes the subscription's guard. */
  onSnapshot: (snapshot: T) => void;
  /** Called on connect↔disconnect transitions (drives the daemon-down hint). */
  onStatus?: (connected: boolean) => void;
  /** Called with a human-readable message on each recoverable drop. */
  onError?: (message: string) => void;
  /**
   * Called when the daemon *answers* but refuses the op — the version-skew
   * signal. A daemon too old to stream this op replies `{ ok: false, error:
   * "unknown <svc> op: subscribe" }` and then holds the connection open, so
   * without this the client would wait forever on a stream that will never
   * arrive. The subscription stops after firing it (no reconnect); the caller
   * falls back to polling. Omit it to keep the original behaviour of ignoring
   * every non-`ok` frame.
   */
  onUnsupported?: (message: string) => void;
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

/** Back-compat alias: `TreeSubscription`'s options before it was generalized. */
export type TreeSubscriptionOptions = SubscriptionOptions<TreeSnapshot>;

const DEFAULT_INITIAL_BACKOFF_MS = 500;
const DEFAULT_MAX_BACKOFF_MS = 10_000;

/**
 * A resilient subscription to one of the daemon's push streams. Construct it,
 * then call {@link start}; call {@link close} to tear it down (idempotent, safe
 * to hand to `context.subscriptions`).
 *
 * Subclasses supply the request envelope and the guard that recognizes a
 * well-formed snapshot for that stream.
 */
export class DaemonSubscription<T> {
  private readonly onSnapshot: (snapshot: T) => void;
  private readonly onStatus?: (connected: boolean) => void;
  private readonly onError?: (message: string) => void;
  private readonly onUnsupported?: (message: string) => void;
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
    private readonly envelope: Envelope,
    private readonly isSnapshot: (payload: Record<string, unknown>) => boolean,
    options: SubscriptionOptions<T>,
  ) {
    this.onSnapshot = options.onSnapshot;
    this.onStatus = options.onStatus;
    this.onError = options.onError;
    this.onUnsupported = options.onUnsupported;
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
    // Re-`start()`ing a subscription that fell back (or was closed) revives it,
    // so a caller can retry after a daemon restart without rebuilding it.
    this.closed = false;
    this.backoff = this.initialBackoffMs;
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
      conn.write(JSON.stringify(this.envelope) + "\n");
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
    // An explicit error reply means the daemon is up but will not serve this op
    // — almost always a daemon too old to know it. Hand that to the caller so it
    // can degrade, and stop: reconnecting would only re-earn the same refusal.
    if (!reply.ok && this.onUnsupported) {
      const message = reply.error ?? "daemon refused the subscription";
      this.close();
      this.onUnsupported(message);
      return;
    }
    // Ignore anything that is not a well-formed snapshot for this stream; a
    // fresh snapshot is the only frame the stream should carry.
    if (reply.ok && reply.payload && this.isSnapshot(reply.payload)) {
      // Any successful snapshot proves the daemon is up: reset backoff and, on
      // the first one, announce the connection.
      this.backoff = this.initialBackoffMs;
      if (!this.connected) {
        this.connected = true;
        this.onStatus?.(true);
      }
      this.onSnapshot(reply.payload as T);
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

/**
 * A subscription to the worktrees `tree` stream (#1267): the repo/worktree rows
 * plus the daemon-backed `show_closed` toggle (#1301), so the reader drives both
 * the tree and the show/hide-closed filter from the same authoritative frame.
 */
export class TreeSubscription extends DaemonSubscription<TreeSnapshot> {
  constructor(socketPath: string, options: SubscriptionOptions<TreeSnapshot>) {
    super(socketPath, subscribeEnvelope(), (payload) => Array.isArray(payload.repos), options);
  }
}

/** One pushed frame of the sessions stream — the same body the `list` op serves. */
export interface SessionsSnapshot {
  /** Every Claude session the daemon currently tracks. */
  sessions: SessionEntry[];
}

/**
 * A subscription to the sessions stream (#1414): the live set of Claude sessions,
 * pushed on every state change so every window's cues flip together instead of
 * drifting up to a poll period apart.
 *
 * Pass `onUnsupported` — a daemon predating the op answers but refuses it, and
 * the caller must fall back to polling `list`.
 */
export class SessionsSubscription extends DaemonSubscription<SessionsSnapshot> {
  constructor(socketPath: string, options: SubscriptionOptions<SessionsSnapshot>) {
    super(
      socketPath,
      sessionsSubscribeEnvelope(),
      (payload) => Array.isArray(payload.sessions),
      options,
    );
  }
}
